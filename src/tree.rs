//! Project-tree model builder for the left browser panel.
//! Extracted from main.rs. Node labels switch language via `App::lang_en`
//! (the Chinese string literals below are UI data, not comments).

use crate::{App, AppWindow, TreeRow, id_str};
use slint::{ModelRc, VecModel};
use std::rc::Rc;

/// Rebuild the project-tree model and push it to the UI.
///
/// A render signature guards the rebuild: if nothing visible changed we return
/// early, avoiding a per-frame model swap that would jitter the hover highlight.
pub(crate) fn build_tree(a: &mut App, ui: &AppWindow) {
    // Render signature — rebuild only when something visible changed.
    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        a.project_name.hash(&mut h);
        a.connected.hash(&mut h);
        a.conn_name.hash(&mut h);
        a.running.hash(&mut h);
        a.baud.hash(&mut h);
        // Channel config takes part in the signature so the tree refreshes on edits.
        a.channels.len().hash(&mut h);
        for c in &a.channels {
            c.sw_channel.hash(&mut h);
            c.is_fd.hash(&mut h);
            c.device_type.hash(&mut h);
            c.baud.hash(&mut h);
            c.channel_index.hash(&mut h);
        }
        a.dbcs.len().hash(&mut h);
        a.dbcs
            .iter()
            .map(|d| d.messages().count())
            .sum::<usize>()
            .hash(&mut h);
        a.txs.len().hash(&mut h);
        for s in &a.series {
            s.name.hash(&mut h);
        }
        a.tree_collapsed.len().hash(&mut h);
        let cset = a.tree_collapsed.iter().fold(0u64, |acc, k| {
            acc ^ {
                let mut hh = std::collections::hash_map::DefaultHasher::new();
                k.hash(&mut hh);
                hh.finish()
            }
        });
        cset.hash(&mut h);
        a.lang_en.hash(&mut h); // language switch must rebuild the tree
        h.finish()
    };
    if sig == a.last_tree_sig {
        return;
    }
    a.last_tree_sig = sig;
    let en = a.lang_en;

    let mut t: Vec<TreeRow> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut dbc_indices: Vec<i32> = Vec::new();
    // Append one tree row plus its collapse key (empty for non-expandable rows).
    #[allow(clippy::too_many_arguments)]
    fn push(
        t: &mut Vec<TreeRow>,
        keys: &mut Vec<String>,
        dbc_indices: &mut Vec<i32>,
        level: i32,
        label: &str,
        kind: &str,
        accent: bool,
        key: &str,
        expandable: bool,
        expanded: bool,
    ) {
        t.push(TreeRow {
            level,
            label: label.into(),
            kind: kind.into(),
            accent,
            expandable,
            expanded,
        });
        keys.push(if expandable {
            key.to_string()
        } else {
            String::new()
        });
        dbc_indices.push(-1);
    }
    let project_open = !a.tree_collapsed.contains("project");
    push(
        &mut t,
        &mut keys,
        &mut dbc_indices,
        0,
        &format!(
            "{}: {}",
            "Project",
            a.project_name
        ),
        "project",
        false,
        "project",
        true,
        project_open,
    );
    if project_open {
        let hardware_open = !a.tree_collapsed.contains("hardware");
        push(
            &mut t,
            &mut keys,
            &mut dbc_indices,
            1,
            if en { "Hardware" } else { "硬件设备" },
            "device",
            false,
            "hardware",
            true,
            hardware_open,
        );
        if hardware_open {
            for c in &a.channels {
                push(
                    &mut t,
                    &mut keys,
                    &mut dbc_indices,
                    2,
                    &format!(
                        "CAN{} - {} - {} - {} - {}",
                        c.sw_channel,
                        c.device_type,
                        if c.is_fd { "CANFD" } else { "CAN" },
                        if c.is_fd {
                            format!("{}/{}", c.baud, c.data_baud)
                        } else {
                            c.baud.clone()
                        },
                        if a.running {
                            if en { "Running" } else { "运行中" }
                        } else if en {
                            "Stopped"
                        } else {
                            "停止"
                        }
                    ),
                    "channel",
                    a.running,
                    "",
                    false,
                    false,
                );
            }
        }

        let dbc_open = !a.tree_collapsed.contains("dbc");
        push(
            &mut t,
            &mut keys,
            &mut dbc_indices,
            1,
            if en { "Database" } else { "数据库" },
            "dbc",
            false,
            "dbc",
            true,
            dbc_open,
        );
        if dbc_open {
            if a.dbcs.is_empty() {
                push(
                    &mut t,
                    &mut keys,
                    &mut dbc_indices,
                    2,
                    if en { "(none loaded)" } else { "(未加载)" },
                    "dbc",
                    false,
                    "",
                    false,
                    false,
                );
            } else {
                for (di, d) in a.dbcs.iter().enumerate() {
                    let dbc_key = format!("dbcfile:{di}:{}", d.file_name);
                    let dbc_file_open = a.tree_collapsed.contains(&dbc_key);
                    push(
                        &mut t,
                        &mut keys,
                        &mut dbc_indices,
                        2,
                        &d.file_name,
                        "dbc",
                        true,
                        &dbc_key,
                        true,
                        dbc_file_open,
                    );
                    *dbc_indices.last_mut().unwrap() = di as i32;
                    if dbc_file_open {
                        let mut msgs: Vec<(u32, String)> =
                            d.messages().map(|m| (m.id, m.name.clone())).collect();
                        msgs.sort_by_key(|(id, _)| *id);
                        for (id, name) in msgs.iter().take(30) {
                            push(
                                &mut t,
                                &mut keys,
                                &mut dbc_indices,
                                3,
                                &format!("0x{id:X} {name}"),
                                "msg",
                                false,
                                "",
                                false,
                                false,
                            );
                        }
                    }
                }
            }
        }

        let tx_open = !a.tree_collapsed.contains("tx");
        push(
            &mut t,
            &mut keys,
            &mut dbc_indices,
            1,
            if en { "Send List" } else { "发送列表" },
            "tx",
            false,
            "tx",
            true,
            tx_open,
        );
        if tx_open {
            for tx in &a.txs {
                push(
                    &mut t,
                    &mut keys,
                    &mut dbc_indices,
                    2,
                    &format!("{} {}", tx.name, id_str(tx.id, tx.ext)),
                    "tx",
                    tx.periodic,
                    "",
                    false,
                    false,
                );
            }
        }

        let curve_open = !a.tree_collapsed.contains("curve");
        push(
            &mut t,
            &mut keys,
            &mut dbc_indices,
            1,
            if en { "Chart" } else { "曲线" },
            "curve",
            false,
            "curve",
            true,
            curve_open,
        );
        if curve_open {
            for s in &a.series {
                push(
                    &mut t,
                    &mut keys,
                    &mut dbc_indices,
                    2,
                    &s.name,
                    "curve",
                    false,
                    "",
                    false,
                    false,
                );
            }
        }
    }
    a.tree_row_keys = keys;
    a.tree_dbc_index = dbc_indices;
    // Mark chart-signal leaf rows (level == 2, kind == "curve") for double-click open/highlight.
    a.tree_curve_sig = t
        .iter()
        .map(|r| {
            if r.kind == "curve" && r.level == 2 {
                Some(r.label.to_string())
            } else {
                None
            }
        })
        .collect();
    ui.set_tree(ModelRc::from(Rc::new(VecModel::from(t))));
}
