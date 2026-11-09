//! `/tools` in an interactive terminal: the built-in catalog with both
//! switches. Space flips the resident switch and Tab the on-demand one; each
//! key writes the same file the Desktop Tools page writes and redraws the list.
use crate::{
    terminal::picker::{Entry, Key, Picked, Picker},
    tool_settings::{self, Row, Switch},
};
use anyhow::Result;
use deepseek_code_core::runtime::RuntimePaths;

fn detail(row: &Row) -> String {
    let mut detail = if row.core { "core tool".to_string() } else { String::new() };
    if !row.enabled && !row.on_demand {
        if !detail.is_empty() {
            detail.push_str(" \u{b7} ");
        }
        detail.push_str("unavailable: neither resident nor discoverable");
    }
    detail
}

fn switch_for(key: Key) -> Option<Switch> {
    match key {
        Key::Space => Some(Switch::Enabled),
        Key::Tab => Some(Switch::OnDemand),
        _ => None,
    }
}

pub fn run(paths: &RuntimePaths) -> Result<()> {
    let mut selected = 0;
    let mut notice: Option<String> = None;
    loop {
        let rows = tool_settings::rows(paths)?;
        let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
        let entries: Vec<Entry> = rows
            .iter()
            .map(|row| Entry {
                title: format!("{:<width$}  {}", row.name, row.status(), width = width),
                detail: detail(row),
            })
            .collect();
        let mut subtitle = format!("{} tools", rows.len());
        if let Some(message) = &notice {
            subtitle.push_str(" \u{b7} ");
            subtitle.push_str(message);
        }
        let picker = Picker {
            heading: "Tool loading",
            subtitle: Some(&subtitle),
            entries: &entries,
            search: true,
            hint: "Space: enabled \u{b7} Tab: on demand \u{b7} Esc to close",
            selected: selected.min(rows.len().saturating_sub(1)),
        };
        match picker.run_keys(&[Key::Space, Key::Tab])? {
            Picked::Key(key, Some(index)) => {
                selected = index;
                let row = &rows[index];
                let Some(switch) = switch_for(key) else { return Ok(()) };
                let current = match switch {
                    Switch::Enabled => row.enabled,
                    Switch::OnDemand => row.on_demand,
                };
                notice = Some(match tool_settings::set(paths, &row.name, switch, !current) {
                    Ok((name, saved)) => format!("{name}: {}", tool_settings::summary(saved.loading(&name))),
                    Err(error) => format!("{error:#}"),
                });
            }
            // Enter has no action here; only Esc closes the panel.
            Picked::Entry(index) => selected = index,
            // A key pressed while the filter matches nothing changes nothing.
            Picked::Key(_, None) => continue,
            Picked::Cancel => return Ok(()),
        }
    }
}
