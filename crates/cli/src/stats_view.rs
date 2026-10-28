//! The Stats tab of the settings dialog, as in the reference CLI: a year of
//! activity as a heatmap, then usage over all time, 30 or 7 days.
use crate::session::Session;
use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One recorded model request.
#[derive(Clone, Debug)]
pub struct Request {
    pub day: NaiveDate,
    pub model: String,
    pub conversation: String,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub tokens: [i64; 4],
}

/// Every request this data directory recorded, across sessions.
pub fn load(session: &Session) -> Result<Vec<Request>> {
    session.store.db.with_conn(|conn| -> Result<Vec<Request>> {
        let mut statement = conn.prepare(
            "SELECT started_at_ms, COALESCE(ended_at_ms, started_at_ms), COALESCE(model,''), conversation_id, COALESCE(input_tokens,0), COALESCE(output_tokens,0), COALESCE(cache_read_tokens,0), COALESCE(cache_write_tokens,0) FROM model_requests WHERE started_at_ms IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, [row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?]))
        })?;
        let mut requests = Vec::new();
        for row in rows {
            let (started_ms, ended_ms, model, conversation, tokens) = row?;
            let Some(time) = chrono::DateTime::from_timestamp_millis(started_ms) else { continue };
            requests.push(Request { day: time.with_timezone(&chrono::Local).date_naive(), model, conversation, started_ms, ended_ms, tokens });
        }
        Ok(requests)
    })?
}

pub const RANGES: [(&str, Option<i64>); 3] = [("All time", None), ("Last 7 days", Some(7)), ("Last 30 days", Some(30))];

/// `1.2m`, `469.5k` or `950`, as the reference CLI's stats.
fn short(tokens: i64) -> String {
    let format = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match tokens {
        ..=999 => tokens.max(0).to_string(),
        1000..=999_999 => format(tokens as f64 / 1000.0, "k"),
        _ => format(tokens as f64 / 1_000_000.0, "m"),
    }
}

fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// A year of daily activity: rows Monday to Sunday, one column a week,
/// shaded by how much was used that day.
fn heatmap(requests: &[Request], today: NaiveDate) -> Vec<String> {
    let mut per_day: HashMap<NaiveDate, i64> = HashMap::new();
    for request in requests {
        *per_day.entry(request.day).or_default() += request.tokens.iter().sum::<i64>().max(1);
    }
    let mut values: Vec<i64> = per_day.values().copied().collect();
    values.sort();
    let level = |value: i64| -> char {
        if value == 0 || values.is_empty() {
            return '\u{b7}';
        }
        let rank = values.partition_point(|other| *other < value) as f64 / values.len() as f64;
        ['\u{2591}', '\u{2592}', '\u{2593}', '\u{2588}'][((rank * 4.0) as usize).min(3)]
    };
    let weeks = 52;
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let first = monday - Duration::weeks(weeks - 1);
    let mut months = String::from("      ");
    let mut last_month = 0;
    for week in 0..weeks {
        let day = first + Duration::weeks(week);
        if day.month() != last_month && day.day() <= 7 {
            last_month = day.month();
            let name = day.format("%b").to_string();
            if months.chars().count() <= 6 + week as usize {
                while months.chars().count() < 6 + week as usize {
                    months.push(' ');
                }
                months.push_str(&name);
            }
        }
    }
    let mut rows = vec![months];
    for weekday in 0..7 {
        let label = match weekday {
            0 => "Mon",
            2 => "Wed",
            4 => "Fri",
            _ => "",
        };
        let mut row = format!("{label:<6}");
        for week in 0..weeks {
            let day = first + Duration::weeks(week) + Duration::days(weekday);
            row.push(if day > today { ' ' } else { level(per_day.get(&day).copied().unwrap_or(0)) });
        }
        rows.push(row.trim_end().to_owned());
    }
    rows.push("      Less \u{2591} \u{2592} \u{2593} \u{2588} More".into());
    rows
}

fn streaks(days: &BTreeSet<NaiveDate>, today: NaiveDate) -> (i64, i64) {
    let mut longest = 0;
    let mut run = 0;
    let mut previous: Option<NaiveDate> = None;
    for day in days {
        run = if previous.is_some_and(|previous| *day - previous == Duration::days(1)) { run + 1 } else { 1 };
        longest = longest.max(run);
        previous = Some(*day);
    }
    let mut current = 0;
    let mut day = if days.contains(&today) { today } else { today - Duration::days(1) };
    while days.contains(&day) {
        current += 1;
        day -= Duration::days(1);
    }
    (longest, current)
}

/// How much text the tokens amount to, as the reference CLI compares it.
fn comparison(tokens: i64) -> Option<String> {
    const BOOKS: [(&str, i64); 5] = [
        ("The Little Prince", 22_000),
        ("The Great Gatsby", 64_000),
        ("Pride and Prejudice", 160_000),
        ("Anna Karenina", 460_000),
        ("War and Peace", 780_000),
    ];
    let (title, size) = BOOKS.iter().rev().find(|(_, size)| tokens >= size * 8 / 10)?;
    let times = tokens as f64 / *size as f64;
    Some(if times < 1.5 {
        format!("Your input and output are about as many tokens as {title}")
    } else {
        format!("Your input and output are about {times:.0}\u{d7} as many tokens as {title}")
    })
}

/// The tab's rows for one range.
pub fn rows(requests: &[Request], range: usize, today: NaiveDate) -> Vec<String> {
    let mut rows = vec!["Overview".to_owned()];
    rows.extend(heatmap(requests, today));
    let names: Vec<String> = RANGES.iter().enumerate().map(|(index, (name, _))| if index == range { format!("[{name}]") } else { name.to_string() }).collect();
    rows.push(names.join(" \u{b7} "));
    let chosen: Vec<&Request> = match RANGES[range].1 {
        Some(days) => requests.iter().filter(|request| request.day > today - Duration::days(days)).collect(),
        None => requests.iter().collect(),
    };
    if chosen.is_empty() {
        rows.push("No usage recorded in this range yet.".into());
        return rows;
    }
    let total = |request: &Request| request.tokens.iter().sum::<i64>();
    let mut per_model: BTreeMap<&str, i64> = BTreeMap::new();
    let mut per_session: HashMap<&str, (i64, i64)> = HashMap::new();
    let mut per_day: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    let mut sums = [0i64; 4];
    for request in &chosen {
        *per_model.entry(request.model.as_str()).or_default() += total(request);
        let span = per_session.entry(request.conversation.as_str()).or_insert((request.started_ms, request.ended_ms));
        span.0 = span.0.min(request.started_ms);
        span.1 = span.1.max(request.ended_ms);
        *per_day.entry(request.day).or_default() += total(request);
        for (sum, value) in sums.iter_mut().zip(request.tokens) {
            *sum += value;
        }
    }
    let favorite = per_model.iter().max_by_key(|(_, tokens)| **tokens).map(|(model, _)| model.to_string()).unwrap_or_default();
    let longest = per_session.values().map(|(start, end)| end - start).max().unwrap_or(0);
    let days: BTreeSet<NaiveDate> = per_day.keys().copied().collect();
    let first = *days.iter().next().unwrap();
    let span = (today - first).num_days() + 1;
    let span = RANGES[range].1.map_or(span, |days| days.min(span));
    let (longest_streak, current_streak) = streaks(&days, today);
    let busiest = per_day.iter().max_by_key(|(_, tokens)| **tokens).map(|(day, _)| day.format("%b %-d").to_string()).unwrap_or_default();
    let plural = |count: i64| if count == 1 { "" } else { "s" };
    let pair = |left: String, right: String| format!("{left:<32}{right}");
    let favorite: String = if favorite.chars().count() > 14 { format!("{}\u{2026}", favorite.chars().take(13).collect::<String>()) } else { favorite };
    rows.push(pair(format!("Favorite model: {favorite}"), format!("Total tokens: {}", short(sums.iter().sum()))));
    rows.push(pair(format!("Sessions: {}", per_session.len()), format!("Longest session: {}", duration(longest))));
    rows.push(pair(format!("Active days: {}/{span}", days.len()), format!("Longest streak: {longest_streak} day{}", plural(longest_streak))));
    rows.push(pair(format!("Most active day: {busiest}"), format!("Current streak: {current_streak} day{}", plural(current_streak))));
    rows.push(format!("Input {} \u{b7} Output {} \u{b7} Cache read {} \u{b7} Cache write {}", short(sums[0]), short(sums[1]), short(sums[2]), short(sums[3])));
    if let Some(line) = comparison(sums[0] + sums[1]) {
        rows.push(line);
    }
    rows.push(String::new());
    rows.push("Models".into());
    let all: i64 = per_model.values().sum::<i64>().max(1);
    let mut models: Vec<_> = per_model.into_iter().collect();
    models.sort_by_key(|(_, tokens)| -tokens);
    for (model, tokens) in models {
        rows.push(format!("  {model:<24}{:>8} tokens  {:>5.1}%", short(tokens), tokens as f64 * 100.0 / all as f64));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(day: NaiveDate, model: &str, session: &str, tokens: i64) -> Request {
        Request { day, model: model.into(), conversation: session.into(), started_ms: 0, ended_ms: 90_000, tokens: [tokens, tokens / 10, 0, 0] }
    }

    #[test]
    fn stats_summarise_ranges_streaks_and_models() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
        let requests = vec![
            request(today, "deepseek-flash", "a", 400_000),
            request(today - Duration::days(1), "deepseek-flash", "a", 50_000),
            request(today - Duration::days(40), "deepseek-v4-pro", "b", 10_000),
        ];
        let all = rows(&requests, 0, today);
        assert_eq!(all[0], "Overview");
        assert!(all[1].starts_with("      "), "month header");
        assert!(all.iter().any(|row| row.starts_with("Favorite model: deepseek-flash")));
        assert!(all.iter().any(|row| row.contains("Sessions: 2") && row.contains("Longest session: 1m 30s")));
        assert!(all.iter().any(|row| row.contains("Longest streak: 2 days") ));
        assert!(all.iter().any(|row| row.contains("Current streak: 2 days")));
        assert!(all.iter().any(|row| row.contains("[All time]")));
        assert!(all.iter().any(|row| row.contains("Anna Karenina")), "{all:#?}");
        let week = rows(&requests, 1, today);
        assert!(week.iter().any(|row| row.contains("Sessions: 1")));
        assert!(!week.iter().any(|row| row.contains("deepseek-v4-pro")));
        assert_eq!(short(1_234_000), "1.2m");
        assert_eq!(short(469_500), "469.5k");
    }
}
