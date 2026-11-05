//! Table-consistency check of a Unicode PST. It verifies internal PST consistency, not fidelity to the manifest.
//!
//! Reads a PST with the crate's upstream read API and checks the tables against the property contexts:
//! - every folder's PidTagContentCount / PidTagContentUnreadCount (property context) against the
//!   contents-table row count and an unread recount from each message's PidTagMessageFlags;
//! - the parent hierarchy-table row cells 0x3602 / 0x3603 / 0x3001 against the folder's PC;
//! - every contents-table cell against the message's PC (all columns; the ids in `B1_IDS` are flagged).
use pstsmith::ltp::prop_context::PropertyValue;
use pstsmith::ltp::table_context::{TableContext, TableRowId};
use pstsmith::messaging::store::Store;
use pstsmith::ndb::node_id::{NodeId, NodeIdType};
use std::collections::BTreeMap;
use std::error::Error;
use std::rc::Rc;

const B1_IDS: &[u16] = &[
    0x0017, 0x0039, 0x0064, 0x0065, 0x0C1E, 0x0E06, 0x0E07, 0x3008, 0x300B,
];

fn show(v: &PropertyValue) -> String {
    match v {
        PropertyValue::Unicode(s) => format!("U{:?}", s.to_string()),
        PropertyValue::String8(s) => format!("S{:?}", s.to_string()),
        PropertyValue::Binary(b) => format!("B{:02X?}", b.buffer()),
        other => format!("{other:?}"),
    }
}

fn rows_in_heap(t: &dyn TableContext) -> bool {
    t.context()
        .rows()
        .is_some_and(|n| matches!(n.id_type(), Ok(NodeIdType::HeapNode)))
}

fn cell(
    t: &dyn TableContext,
    row_id: u32,
    prop: u16,
) -> Result<Option<Option<PropertyValue>>, String> {
    // Ok(None): no such column; Ok(Some(None)): column without a cell; Ok(Some(Some(v))): value.
    let ctx = t.context();
    let row = t
        .find_row(TableRowId::new(row_id))
        .map_err(|e| format!("find_row {row_id:X}: {e:?}"))?;
    let values = row.columns(ctx).map_err(|e| format!("columns: {e:?}"))?;
    for (col, val) in ctx.columns().iter().zip(values) {
        if col.prop_id() != prop {
            continue;
        }
        return Ok(Some(match val {
            None => None,
            Some(v) => Some(
                t.read_column(&v, col.prop_type())
                    .map_err(|e| format!("read_column {prop:04X}: {e:?}"))?,
            ),
        }));
    }
    Ok(None)
}

struct Tally {
    mismatches: Vec<String>,
    tc_checked: BTreeMap<u16, (usize, usize)>, // id -> (compared, mismatched)
    messages: usize,
    unread_messages: usize,
    no_submit: usize,
}

fn walk(
    store: &Rc<dyn Store>,
    parent: NodeId,
    path: &mut Vec<String>,
    t: &mut Tally,
) -> Result<(), Box<dyn Error>> {
    let parent_folder = store.open_folder(&store.properties().make_entry_id(parent)?)?;
    let Some(hier) = parent_folder.hierarchy_table() else {
        println!("FOLDER-HIER {:?}: no hierarchy table", path);
        return Ok(());
    };
    let counter_cols: Vec<String> = hier
        .context()
        .columns()
        .iter()
        .filter(|c| matches!(c.prop_id(), 0x3602 | 0x3603))
        .map(|c| format!("{:04X}:{:?}", c.prop_id(), c.prop_type()))
        .collect();
    println!(
        "HIER-TABLE of {:?}: columns={} counter_columns={:?} rows_in_heap={} rows={}",
        path,
        hier.context().columns().len(),
        counter_cols,
        rows_in_heap(hier.as_ref()),
        hier.rows_matrix().count()
    );
    if counter_cols.len() == 1 {
        t.mismatches.push(format!(
            "{path:?}: hierarchy table has only one counter column {counter_cols:?}"
        ));
    }
    let children: Vec<u32> = hier.rows_matrix().map(|r| u32::from(r.id())).collect();
    for child in children {
        let node = NodeId::from(child);
        let folder = store.open_folder(&store.properties().make_entry_id(node)?)?;
        let props = folder.properties();
        let name = props
            .display_name()
            .unwrap_or_else(|e| format!("<name error {e}>"));
        path.push(name.clone());
        let pc_content = props.get(0x3602).map(show);
        let pc_unread = props.get(0x3603).map(show);
        let pc_name = props.get(0x3001).map(show);
        let pc_subfolders = props.get(0x360A).map(show);

        // Contents rows and unread recount from each message's own PidTagMessageFlags.
        let mut rows = 0usize;
        let mut unread = 0usize;
        if let Some(contents) = folder.contents_table() {
            let ctx = contents.context();
            for row in contents.rows_matrix() {
                rows += 1;
                t.messages += 1;
                let row_id = u32::from(row.id());
                let message = store.open_message(
                    &store.properties().make_entry_id(NodeId::from(row_id))?,
                    None,
                )?;
                let mprops = message.properties();
                let flags = match mprops.get(0x0E07) {
                    Some(PropertyValue::Integer32(f)) => *f,
                    other => {
                        t.mismatches
                            .push(format!("{path:?} msg {row_id:X}: PC 0E07 = {other:?}"));
                        0
                    }
                };
                if flags & 1 == 0 {
                    unread += 1;
                    t.unread_messages += 1;
                }
                if mprops.get(0x0039).is_none() {
                    t.no_submit += 1;
                }
                let values = row.columns(ctx)?;
                for (col, val) in ctx.columns().iter().zip(values) {
                    let id = col.prop_id();
                    if matches!(id, 0x67F2 | 0x67F3) {
                        continue; // LTP row id / version, not message properties
                    }
                    let tc = match val {
                        None => None,
                        Some(v) => match contents.read_column(&v, col.prop_type()) {
                            Ok(v) => Some(show(&v)),
                            Err(e) => Some(format!("<read error {e:?}>")),
                        },
                    };
                    let pc = mprops.get(id).map(show);
                    let entry = t.tc_checked.entry(id).or_default();
                    entry.0 += 1;
                    if tc != pc {
                        entry.1 += 1;
                        if B1_IDS.contains(&id) {
                            t.mismatches.push(format!(
                                "{path:?} msg {row_id:X}: TC {id:04X} = {tc:?} vs PC {pc:?}"
                            ));
                        }
                    }
                }
            }
        } else {
            println!("FOLDER {path:?}: no contents table");
        }

        let mut hier_cells = Vec::new();
        for prop in [0x3602u16, 0x3603, 0x3001, 0x360A] {
            let c = match cell(hier.as_ref(), child, prop) {
                Ok(None) => "no-column".to_string(),
                Ok(Some(None)) => "no-cell".to_string(),
                Ok(Some(Some(v))) => show(&v),
                Err(e) => format!("<{e}>"),
            };
            let pc = match prop {
                0x3602 => pc_content.clone(),
                0x3603 => pc_unread.clone(),
                0x360A => pc_subfolders.clone(),
                _ => pc_name.clone(),
            };
            if c != "no-column" && Some(c.clone()) != pc {
                t.mismatches.push(format!(
                    "{path:?}: hierarchy row cell {prop:04X} = {c} vs PC {pc:?}"
                ));
            }
            hier_cells.push(format!("{prop:04X}={c}"));
        }
        let want_content = Some(format!("{:?}", PropertyValue::Integer32(rows as i32)));
        let want_unread = Some(format!("{:?}", PropertyValue::Integer32(unread as i32)));
        if pc_content != want_content {
            t.mismatches.push(format!(
                "{path:?}: PC 3602 = {pc_content:?} but {rows} contents rows"
            ));
        }
        if pc_unread != want_unread {
            t.mismatches.push(format!(
                "{path:?}: PC 3603 = {pc_unread:?} but {unread} unread messages (recount)"
            ));
        }
        println!(
            "FOLDER {path:?} node={child:X} PC3602={pc_content:?} PC3603={pc_unread:?} rows={rows} unread_recount={unread} parent_row[{}]",
            hier_cells.join(" ")
        );
        walk(store, node, path, t)?;
        path.pop();
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: pst-table-check FILE.pst")?;
    let store = pstsmith::open_store(&path)?;
    let ipm = store.properties().ipm_sub_tree_entry_id()?.node_id();
    let mut t = Tally {
        mismatches: Vec::new(),
        tc_checked: BTreeMap::new(),
        messages: 0,
        unread_messages: 0,
        no_submit: 0,
    };
    walk(&store, ipm, &mut Vec::new(), &mut t)?;
    for (id, (n, bad)) in &t.tc_checked {
        println!(
            "TC-COLUMN {id:04X}{} compared={n} mismatched={bad}",
            if B1_IDS.contains(id) { " (B1)" } else { "" }
        );
    }
    println!(
        "MESSAGES {} unread={} without_0039={}",
        t.messages, t.unread_messages, t.no_submit
    );
    for m in &t.mismatches {
        println!("MISMATCH {m}");
    }
    println!("RESULT mismatches={}", t.mismatches.len());
    if !t.mismatches.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
