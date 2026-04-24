//! Column-visibility mapping tests. Exercises the pure-logic helpers in
//! `navigator_gui::listview` that turn a `Columns` config into the
//! ordered list of visible columns and the `iSubItem → logical` map.

#![cfg(windows)]

use navigator_config::Columns;
use navigator_gui::listview::{column_for_subitem, visible_columns, LogicalColumn};

#[test]
fn default_columns_show_every_entry() {
    let cols = Columns::default();
    assert_eq!(
        visible_columns(&cols),
        vec![
            LogicalColumn::Name,
            LogicalColumn::Size,
            LogicalColumn::Type,
            LogicalColumn::Modified,
        ],
    );
}

#[test]
fn name_column_is_always_first() {
    let cols = Columns { show_size: false, show_type: false, show_modified: false };
    let v = visible_columns(&cols);
    assert_eq!(v, vec![LogicalColumn::Name]);
    assert_eq!(column_for_subitem(&cols, 0), Some(LogicalColumn::Name));
    assert_eq!(column_for_subitem(&cols, 1), None);
}

#[test]
fn hiding_type_shifts_modified_left() {
    let cols = Columns { show_size: true, show_type: false, show_modified: true };
    assert_eq!(
        visible_columns(&cols),
        vec![LogicalColumn::Name, LogicalColumn::Size, LogicalColumn::Modified],
    );
    assert_eq!(column_for_subitem(&cols, 2), Some(LogicalColumn::Modified));
}

#[test]
fn subitem_mapping_is_dense_and_ordered() {
    // All combinations — the `Name` column always maps back to
    // LogicalColumn::Name at index 0, and the optional columns keep
    // canonical Size/Type/Modified order.
    for size in [true, false] {
        for ty in [true, false] {
            for modified in [true, false] {
                let cols = Columns { show_size: size, show_type: ty, show_modified: modified };
                let v = visible_columns(&cols);
                assert_eq!(v[0], LogicalColumn::Name);
                // Dense mapping: every iSubItem from 0..v.len() resolves.
                for (i, expected) in v.iter().enumerate() {
                    assert_eq!(column_for_subitem(&cols, i as i32), Some(*expected));
                }
                // Out-of-range returns None instead of wrapping.
                assert_eq!(column_for_subitem(&cols, v.len() as i32), None);
                // Negative indices are treated as no-such-column.
                assert_eq!(column_for_subitem(&cols, -1), None);
            }
        }
    }
}

#[test]
fn columns_default_is_fully_enabled() {
    let c = Columns::default();
    assert!(c.show_size);
    assert!(c.show_type);
    assert!(c.show_modified);
}
