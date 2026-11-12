//! Rust-side JSON boundary validation for values crossing between the user
//! script and the host.
//!
//! Validation deliberately does not use `JSON.stringify`, `Object.keys`, or any
//! other user-replaceable intrinsic. It walks the engine value directly and
//! enforces depth, cycle, plain-object, and byte limits while converting.

use rquickjs::{qjs, Exception, Filter, Value as JsValue};
use serde_json::{Map, Number, Value};

/// The largest property name that JavaScript treats as an array index is
/// `2^32 - 2`; `2^32 - 1` is an ordinary string property on arrays.
const MAX_ARRAY_INDEX: u32 = u32::MAX - 1;

/// `i64::MIN` is exactly representable as `f64`.
const I64_MIN_AS_F64: f64 = -9_223_372_036_854_775_808.0;
/// `2^63`, used as an exclusive upper bound. `i64::MAX as f64` rounds up to
/// this value, so using it as an inclusive bound would admit `2^63` and then
/// saturate the cast to `i64::MAX`.
const I64_UPPER_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;

/// Why a JavaScript value cannot cross the JSON boundary.
#[derive(Debug)]
pub(super) enum BoundaryError {
    NonJson { kind: &'static str },
    NonPlainObject,
    NonFiniteNumber,
    Cyclic,
    TooDeep { limit: usize },
    TooLarge { limit: usize, actual: usize },
    ReadFailure { message: String },
}

impl BoundaryError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            BoundaryError::NonJson { .. } | BoundaryError::NonPlainObject => "non_json_value",
            BoundaryError::NonFiniteNumber => "invalid_number",
            BoundaryError::Cyclic => "cyclic_value",
            BoundaryError::TooDeep { .. } => "value_too_deep",
            BoundaryError::TooLarge { .. } => "value_too_large",
            BoundaryError::ReadFailure { .. } => "unreadable_value",
        }
    }

    pub(super) fn message(&self, label: &str) -> String {
        match self {
            BoundaryError::NonJson { kind } => format!("{label} contains a {kind} value"),
            BoundaryError::NonPlainObject => format!("{label} contains a non-plain object"),
            BoundaryError::NonFiniteNumber => format!("{label} contains a non-finite number"),
            BoundaryError::Cyclic => format!("{label} contains a cycle"),
            BoundaryError::TooDeep { limit } => {
                format!("{label} nests deeper than {limit} levels")
            }
            BoundaryError::TooLarge { limit, actual } => {
                format!("{label} is at least {actual} bytes, above the {limit}-byte limit")
            }
            BoundaryError::ReadFailure { message } => {
                format!("{label} could not be read: {message}")
            }
        }
    }
}

/// Converts a JS value into strict JSON.
///
/// `trusted_prototype` is the realm's original `Object.prototype`, captured
/// before user code ran. Plain objects must have that prototype or no prototype.
/// Arrays must have every index `0..length-1` as an own property; holes,
/// accessor-mediated prototype fallback, proxies, functions, symbols, bigints,
/// promises, modules, errors, cycles, non-finite numbers, and `undefined` are
/// rejected.
pub(super) fn js_to_json<'js>(
    value: JsValue<'js>,
    trusted_prototype: &JsValue<'js>,
    max_depth: usize,
    max_bytes: usize,
) -> Result<Value, BoundaryError> {
    let mut converter = Converter {
        trusted_prototype,
        max_depth,
        limit: max_bytes,
        used: 0,
        visited: Vec::new(),
    };
    let json = converter.convert(value, 0)?;
    // The incremental accounting mirrors serde_json's output length. Keep an
    // exact check as the authority in case escaping or numbers differ.
    let bytes = serde_json::to_vec(&json).map_err(|_| BoundaryError::ReadFailure {
        message: "cannot serialize the JSON value".to_owned(),
    })?;
    if bytes.len() > max_bytes {
        return Err(BoundaryError::TooLarge {
            limit: max_bytes,
            actual: bytes.len(),
        });
    }
    Ok(json)
}

struct Converter<'js, 'p> {
    trusted_prototype: &'p JsValue<'js>,
    max_depth: usize,
    limit: usize,
    used: usize,
    visited: Vec<JsValue<'js>>,
}

impl<'js> Converter<'js, '_> {
    fn convert(&mut self, value: JsValue<'js>, depth: usize) -> Result<Value, BoundaryError> {
        if depth > self.max_depth {
            return Err(BoundaryError::TooDeep {
                limit: self.max_depth,
            });
        }
        if value.is_undefined() {
            return Err(BoundaryError::NonJson { kind: "undefined" });
        }
        if value.is_null() {
            self.charge(4)?;
            return Ok(Value::Null);
        }
        if value.is_bool() {
            self.charge(if value.as_bool() == Some(true) { 4 } else { 5 })?;
            return Ok(Value::Bool(value.as_bool().unwrap_or(false)));
        }
        if value.is_int() {
            let number = Number::from(value.as_int().unwrap_or(0));
            self.charge(number.to_string().len())?;
            return Ok(Value::Number(number));
        }
        if value.is_float() {
            let number = value.as_float().unwrap_or(f64::NAN);
            let json = if number.is_finite()
                && number.fract() == 0.0
                && number >= I64_MIN_AS_F64
                && number < I64_UPPER_EXCLUSIVE_AS_F64
            {
                // The value is an exact i64, so the cast cannot saturate and
                // does not change the numeric value. 2^63 stays a float.
                Number::from(number as i64)
            } else {
                Number::from_f64(number).ok_or(BoundaryError::NonFiniteNumber)?
            };
            self.charge(json.to_string().len())?;
            return Ok(Value::Number(json));
        }
        if value.is_string() {
            return self.convert_string(value);
        }
        // Check proxies before any structured walk. `JS_IsArray` and
        // `JS_IsProxy` are class-id checks and do not run traps, but rejecting
        // first guarantees no proxy ever reaches enumeration or property reads.
        if value.is_proxy() {
            return Err(BoundaryError::NonJson { kind: "proxy" });
        }
        if value.is_array() {
            return self.convert_array(value, depth);
        }
        if value.is_function() || value.is_constructor() {
            return Err(BoundaryError::NonJson { kind: "function" });
        }
        if value.is_symbol() {
            return Err(BoundaryError::NonJson { kind: "symbol" });
        }
        if value.is_big_int() {
            return Err(BoundaryError::NonJson { kind: "bigint" });
        }
        if value.is_promise() {
            return Err(BoundaryError::NonJson { kind: "promise" });
        }
        if value.is_exception() || value.is_error() {
            return Err(BoundaryError::NonJson { kind: "error" });
        }
        if value.is_module() {
            return Err(BoundaryError::NonJson { kind: "module" });
        }
        if value.is_object() {
            return self.convert_object(value, depth);
        }
        Err(BoundaryError::NonJson {
            kind: "unreadable value",
        })
    }

    fn convert_string(&mut self, value: JsValue<'js>) -> Result<Value, BoundaryError> {
        let converted = value.as_string().expect("checked string").to_string();
        let text = converted.map_err(|_| read_failure(&value))?;
        let remaining = self.limit.saturating_sub(self.used);
        if text.len() > remaining {
            return Err(BoundaryError::TooLarge {
                limit: self.limit,
                actual: self.used.saturating_add(text.len()),
            });
        }
        let serialized = serde_json::to_string(&text).map_err(|_| BoundaryError::ReadFailure {
            message: "cannot serialize a string".to_owned(),
        })?;
        self.charge(serialized.len())?;
        Ok(Value::String(text))
    }

    fn convert_array(&mut self, value: JsValue<'js>, depth: usize) -> Result<Value, BoundaryError> {
        if self.visited.iter().any(|seen| seen == &value) {
            return Err(BoundaryError::Cyclic);
        }
        let array = value.as_array().expect("checked array");
        let len = array.len();
        if len > self.limit.saturating_sub(self.used) {
            return Err(BoundaryError::TooLarge {
                limit: self.limit,
                actual: self.used.saturating_add(len),
            });
        }

        // Reject holes without trusting `in`, `Object.keys`, or prototype
        // lookups. QuickJS sorts own numeric keys ascending and stores holes
        // as absent keys, so a contiguous run 0..len is the only accepted
        // shape. `Array::get` would otherwise read an inherited value.
        let mut next_expected = 0usize;
        for key in array
            .as_object()
            .own_keys::<std::string::String>(Filter::new().string())
        {
            let key = key.map_err(|_| read_failure(&value))?;
            let Some(index) = parse_array_index(&key) else {
                continue;
            };
            let index = index as usize;
            if index != next_expected || index >= len {
                return Err(BoundaryError::NonJson {
                    kind: "sparse array",
                });
            }
            next_expected += 1;
        }
        if next_expected != len {
            return Err(BoundaryError::NonJson {
                kind: "sparse array",
            });
        }

        self.visited.push(value.clone());
        self.charge(2)?;
        let mut out = Vec::with_capacity(len.min(1024));
        for index in 0..len {
            if index > 0 {
                self.charge(1)?;
            }
            // The key scan above happens before any value is read. A getter for
            // an earlier element can delete a later one and expose an inherited
            // `Array.prototype[index]`, so ownership is re-checked natively
            // immediately before each read. Nothing runs user code between this
            // check and `get`, and the check does not invoke getters.
            if !has_own_index(&value, index)? {
                return Err(BoundaryError::NonJson {
                    kind: "sparse array",
                });
            }
            let item = array
                .get::<JsValue>(index)
                .map_err(|_| read_failure(&value))?;
            out.push(self.convert(item, depth + 1)?);
        }
        self.visited.pop();
        Ok(Value::Array(out))
    }

    fn convert_object(
        &mut self,
        value: JsValue<'js>,
        depth: usize,
    ) -> Result<Value, BoundaryError> {
        let object = value.as_object().expect("checked object");
        if let Some(prototype) = object.get_prototype() {
            if prototype.as_value() != self.trusted_prototype {
                return Err(BoundaryError::NonPlainObject);
            }
        }
        if self.visited.iter().any(|seen| seen == &value) {
            return Err(BoundaryError::Cyclic);
        }
        self.visited.push(value.clone());
        self.charge(2)?;
        let mut map = Map::new();
        let mut first = true;
        for key in object.keys::<std::string::String>() {
            let key = key.map_err(|_| read_failure(&value))?;
            let serialized_key =
                serde_json::to_string(&key).map_err(|_| BoundaryError::ReadFailure {
                    message: "cannot serialize an object key".to_owned(),
                })?;
            if !first {
                self.charge(1)?;
            }
            self.charge(serialized_key.len() + 1)?;
            let item = object
                .get::<_, JsValue>(key.as_str())
                .map_err(|_| read_failure(&value))?;
            map.insert(key, self.convert(item, depth + 1)?);
            first = false;
        }
        self.visited.pop();
        Ok(Value::Object(map))
    }

    fn charge(&mut self, bytes: usize) -> Result<(), BoundaryError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.limit {
            Err(BoundaryError::TooLarge {
                limit: self.limit,
                actual: self.used,
            })
        } else {
            Ok(())
        }
    }
}

/// Returns `Some(index)` only for a canonical JavaScript array index in
/// `0..=2^32 - 2`. Anything else (`"4294967295"`, `"01"`, `"+1"`, `"length"`)
/// is an ordinary non-index property on an array and is ignored, matching
/// `JSON.stringify`.
fn parse_array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        if value > u64::from(MAX_ARRAY_INDEX) {
            return None;
        }
    }
    Some(value as u32)
}

/// Checks that `index` is an own property of `value` right now, without running
/// getters or walking the prototype chain.
///
/// This uses the public raw context/value accessors and is called immediately
/// before each element read. `JS_GetOwnProperty` with a null descriptor does not
/// allocate a descriptor or duplicate values, and arrays have no exotic
/// own-property hook, so no user code runs. Proxies are rejected before this
/// point, which is what would otherwise make the exotic hook reachable.
fn has_own_index(value: &JsValue<'_>, index: usize) -> Result<bool, BoundaryError> {
    let Ok(index) = u32::try_from(index) else {
        return Ok(false);
    };
    let ctx = value.ctx();
    let raw_ctx = ctx.as_raw().as_ptr();
    // SAFETY: `raw_ctx` belongs to the live context that owns `value`, and the
    // atom is released below. JS_NewAtomUInt32 does not run user code.
    let atom = unsafe { qjs::JS_NewAtomUInt32(raw_ctx, index) };
    if atom == qjs::JS_ATOM_NULL {
        let _ = ctx.catch();
        return Err(BoundaryError::ReadFailure {
            message: "cannot inspect an array element".to_owned(),
        });
    }
    // SAFETY: `value` and `raw_ctx` belong to the same context. A null
    // descriptor requests existence only; no descriptor is filled or needs
    // freeing, and this does not invoke getters.
    let exists =
        unsafe { qjs::JS_GetOwnProperty(raw_ctx, std::ptr::null_mut(), value.as_raw(), atom) };
    // SAFETY: the atom was created above and is not used afterwards.
    unsafe { qjs::JS_FreeAtom(raw_ctx, atom) };
    if exists < 0 {
        let _ = ctx.catch();
        return Err(BoundaryError::ReadFailure {
            message: "cannot inspect an array element".to_owned(),
        });
    }
    Ok(exists > 0)
}

/// Clears a pending exception raised while reading a property and turns it into
/// a boundary diagnostic. This prevents a stale JS exception from leaking into
/// later engine operations.
fn read_failure(value: &JsValue<'_>) -> BoundaryError {
    let caught = value.ctx().catch();
    let message = caught
        .as_object()
        .and_then(|object| Exception::from_object(object.clone()))
        .and_then(|exception| exception.message())
        .unwrap_or_else(|| "unreadable value".to_owned());
    BoundaryError::ReadFailure { message }
}
