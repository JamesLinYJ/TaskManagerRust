// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程名称筛选
//
//   文件:       src/pages/processes/filter.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+ x86_64；Rust 1.99.1
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! UI-thread view of an already committed process snapshot.
//!
//! The filter never opens a process, token, or icon, and it does not change
//! what the sampler collects. An empty needle shows every row. A needle with
//! no hits yields an empty visible list; that is not a refresh error.
//! Hidden rows are omitted from the virtual-list model so the owner-data
//! callback never formats them.

use super::SelectionScrollPolicy;
use crate::system::process_identity::ProcIdentity;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ProcessNameFilter {
    needle: String,
}

impl ProcessNameFilter {
    pub(super) fn from_text(raw: &str) -> Self {
        Self {
            needle: raw.trim().to_lowercase(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// `image_name_lower` must already be the lowercase image name, without an
    /// architecture suffix. Matching is a substring test.
    pub(super) fn matches(&self, image_name_lower: &str) -> bool {
        self.needle.is_empty() || image_name_lower.contains(&self.needle)
    }
}

/// Why the visible process list is being rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProcessViewUpdate {
    /// Timer or F5. Keep the current scroll position.
    Refresh,
    /// The user changed the filter text.
    FilterChanged,
    /// An explicit navigation, such as Go To Process, must bring the row into view.
    RevealIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProcessNameView {
    /// Indexes into the unfiltered snapshot, in that snapshot's order.
    pub(super) visible_indices: Vec<usize>,
    /// Identity to restore. Kept while that process remains in the unfiltered
    /// snapshot, including when the active filter hides its row.
    pub(super) identity_cache: Option<ProcIdentity>,
    /// Index into `visible_indices`. `None` when the list has nothing to highlight.
    pub(super) selected_list_index: Option<usize>,
    pub(super) scroll: SelectionScrollPolicy,
}

pub(super) fn plan_process_name_view(
    rows: &[(ProcIdentity, &str)],
    filter: &ProcessNameFilter,
    identity_cache: Option<ProcIdentity>,
    visible_selection: Option<ProcIdentity>,
    neighbor_hint: Option<ProcIdentity>,
    update: ProcessViewUpdate,
) -> ProcessNameView {
    let visible_indices = rows
        .iter()
        .enumerate()
        .filter(|(_, (_, image_name_lower))| filter.matches(image_name_lower))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    let cache_entry = identity_cache.and_then(|identity| {
        entry_index(rows, identity).map(|entry_index| (identity, entry_index))
    });

    if let Some((cached_id, entry_index)) = cache_entry {
        if let Some(list_index) = list_index(&visible_indices, rows, cached_id) {
            return view(
                visible_indices,
                Some(cached_id),
                Some(list_index),
                rows,
                visible_selection,
                update,
            );
        }

        // The cached process is still in the snapshot, but the active filter
        // hides it. Keep the cache and highlight a row the user can see.
        let list_index =
            fallback_list_index(rows, &visible_indices, Some(entry_index), visible_selection);
        return view(
            visible_indices,
            Some(cached_id),
            list_index,
            rows,
            visible_selection,
            update,
        );
    }

    let had_selection = identity_cache.is_some() || visible_selection.is_some();
    let surviving_highlight = visible_selection
        .and_then(|identity| list_index(&visible_indices, rows, identity))
        .or(neighbor_hint.and_then(|identity| list_index(&visible_indices, rows, identity)));
    let list_index = if surviving_highlight.is_some() {
        surviving_highlight
    } else if had_selection && !visible_indices.is_empty() {
        Some(0)
    } else {
        None
    };
    let new_cache = list_index.map(|index| rows[visible_indices[index]].0);
    view(
        visible_indices,
        new_cache,
        list_index,
        rows,
        visible_selection,
        update,
    )
}

pub(super) fn neighbor_after(
    identities: &[ProcIdentity],
    identity: ProcIdentity,
) -> Option<ProcIdentity> {
    let index = identities
        .iter()
        .position(|candidate| *candidate == identity)?;
    identities.get(index + 1).copied().or_else(|| {
        index
            .checked_sub(1)
            .and_then(|previous| identities.get(previous).copied())
    })
}

fn view(
    visible_indices: Vec<usize>,
    identity_cache: Option<ProcIdentity>,
    selected_list_index: Option<usize>,
    rows: &[(ProcIdentity, &str)],
    previous_highlight: Option<ProcIdentity>,
    update: ProcessViewUpdate,
) -> ProcessNameView {
    let highlight = selected_list_index.map(|index| rows[visible_indices[index]].0);
    let reveal_selection = match update {
        ProcessViewUpdate::RevealIdentity => true,
        ProcessViewUpdate::Refresh => false,
        ProcessViewUpdate::FilterChanged => highlight != previous_highlight,
    };
    let scroll = super::refresh_selection_scroll_policy(reveal_selection);
    ProcessNameView {
        visible_indices,
        identity_cache,
        selected_list_index,
        scroll,
    }
}

fn entry_index(rows: &[(ProcIdentity, &str)], identity: ProcIdentity) -> Option<usize> {
    rows.iter()
        .position(|(candidate, _)| *candidate == identity)
}

fn list_index(
    visible_indices: &[usize],
    rows: &[(ProcIdentity, &str)],
    identity: ProcIdentity,
) -> Option<usize> {
    visible_indices
        .iter()
        .position(|&index| rows[index].0 == identity)
}

fn fallback_list_index(
    rows: &[(ProcIdentity, &str)],
    visible_indices: &[usize],
    anchor_entry: Option<usize>,
    preferred: Option<ProcIdentity>,
) -> Option<usize> {
    if let Some(identity) = preferred
        && let Some(index) = list_index(visible_indices, rows, identity)
    {
        return Some(index);
    }
    let Some(anchor) = anchor_entry else {
        return if visible_indices.is_empty() {
            None
        } else {
            Some(0)
        };
    };
    visible_indices
        .iter()
        .enumerate()
        .min_by_key(|(_, entry_index)| entry_index.abs_diff(anchor))
        .map(|(list_index, _)| list_index)
}

#[cfg(test)]
mod tests {
    use super::{ProcessNameFilter, ProcessViewUpdate, neighbor_after, plan_process_name_view};
    use crate::pages::processes::SelectionScrollPolicy;
    use crate::system::process_identity::ProcIdentity;

    struct Row {
        identity: ProcIdentity,
        name: &'static str,
    }

    fn id(pid: u32) -> ProcIdentity {
        ProcIdentity::new(pid, u64::from(pid) + 10)
    }

    fn sample() -> Vec<Row> {
        vec![
            Row {
                identity: id(1),
                name: "alpha.exe",
            },
            Row {
                identity: id(2),
                name: "bravo.exe",
            },
            Row {
                identity: id(3),
                name: "charlie.exe",
            },
        ]
    }

    fn plan(
        rows: &[Row],
        filter: &str,
        cache: Option<ProcIdentity>,
        visible: Option<ProcIdentity>,
        neighbor: Option<ProcIdentity>,
        update: ProcessViewUpdate,
    ) -> super::ProcessNameView {
        let borrowed = rows
            .iter()
            .map(|row| (row.identity, row.name))
            .collect::<Vec<_>>();
        plan_process_name_view(
            &borrowed,
            &ProcessNameFilter::from_text(filter),
            cache,
            visible,
            neighbor,
            update,
        )
    }

    fn highlighted(rows: &[Row], view: &super::ProcessNameView) -> Option<ProcIdentity> {
        view.selected_list_index
            .map(|index| rows[view.visible_indices[index]].identity)
    }

    fn visible_names<'a>(rows: &'a [Row], view: &super::ProcessNameView) -> Vec<&'a str> {
        view.visible_indices
            .iter()
            .map(|&index| rows[index].name)
            .collect()
    }

    #[test]
    fn empty_and_whitespace_filters_show_the_full_list() {
        let rows = sample();
        for filter in ["", "   ", "\t"] {
            let view = plan(
                &rows,
                filter,
                Some(id(2)),
                Some(id(2)),
                None,
                ProcessViewUpdate::Refresh,
            );
            assert_eq!(
                visible_names(&rows, &view),
                ["alpha.exe", "bravo.exe", "charlie.exe"]
            );
            assert_eq!(view.identity_cache, Some(id(2)));
            assert_eq!(highlighted(&rows, &view), Some(id(2)));
            assert_eq!(view.scroll, SelectionScrollPolicy::PreserveViewport);
        }
    }

    #[test]
    fn name_filter_is_a_case_insensitive_image_name_substring() {
        let rows = sample();
        let view = plan(
            &rows,
            "  CHAR  ",
            Some(id(3)),
            Some(id(3)),
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert_eq!(visible_names(&rows, &view), ["charlie.exe"]);
        assert_eq!(highlighted(&rows, &view), Some(id(3)));
        assert!(!ProcessNameFilter::from_text("32").matches("demo.exe"));
        assert!(ProcessNameFilter::from_text("demo").matches("demo.exe"));
    }

    #[test]
    fn no_matches_leave_an_empty_list_and_keep_the_cached_identity() {
        let rows = sample();
        let view = plan(
            &rows,
            "zzz",
            Some(id(2)),
            Some(id(2)),
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert!(view.visible_indices.is_empty());
        assert_eq!(highlighted(&rows, &view), None);
        assert_eq!(view.identity_cache, Some(id(2)));
        assert_eq!(view.scroll, SelectionScrollPolicy::RevealSelection);
    }

    #[test]
    fn hidden_selection_moves_to_a_visible_match_without_dropping_the_cache() {
        let rows = sample();
        let view = plan(
            &rows,
            "c",
            Some(id(2)),
            Some(id(2)),
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert_eq!(visible_names(&rows, &view), ["charlie.exe"]);
        assert_eq!(highlighted(&rows, &view), Some(id(3)));
        assert_eq!(view.identity_cache, Some(id(2)));
        assert_eq!(view.scroll, SelectionScrollPolicy::RevealSelection);
    }

    #[test]
    fn clearing_the_filter_restores_a_cached_identity_that_is_still_present() {
        let rows = sample();
        let view = plan(
            &rows,
            "",
            Some(id(2)),
            Some(id(3)),
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert_eq!(highlighted(&rows, &view), Some(id(2)));
        assert_eq!(view.identity_cache, Some(id(2)));
        assert_eq!(view.scroll, SelectionScrollPolicy::RevealSelection);
    }

    #[test]
    fn refresh_keeps_the_visible_highlight_and_the_hidden_cache() {
        let rows = sample();
        let view = plan(
            &rows,
            "alp",
            Some(id(2)),
            Some(id(1)),
            None,
            ProcessViewUpdate::Refresh,
        );
        assert_eq!(visible_names(&rows, &view), ["alpha.exe"]);
        assert_eq!(highlighted(&rows, &view), Some(id(1)));
        assert_eq!(view.identity_cache, Some(id(2)));
        assert_eq!(view.scroll, SelectionScrollPolicy::PreserveViewport);
    }

    #[test]
    fn a_departed_process_moves_selection_to_its_neighbor() {
        let rows = [
            Row {
                identity: id(1),
                name: "alpha.exe",
            },
            Row {
                identity: id(3),
                name: "charlie.exe",
            },
        ];
        let view = plan(
            &rows,
            "",
            Some(id(2)),
            Some(id(2)),
            Some(id(3)),
            ProcessViewUpdate::Refresh,
        );
        assert_eq!(highlighted(&rows, &view), Some(id(3)));
        assert_eq!(view.identity_cache, Some(id(3)));
        assert_eq!(view.scroll, SelectionScrollPolicy::PreserveViewport);
    }

    #[test]
    fn an_unselected_list_is_not_given_a_selection_by_filtering() {
        let rows = sample();
        let view = plan(
            &rows,
            "alp",
            None,
            None,
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert_eq!(visible_names(&rows, &view), ["alpha.exe"]);
        assert_eq!(highlighted(&rows, &view), None);
        assert_eq!(view.identity_cache, None);
    }

    #[test]
    fn explicit_navigation_reveals_the_cached_process() {
        let rows = sample();
        let view = plan(
            &rows,
            "",
            Some(id(3)),
            Some(id(1)),
            None,
            ProcessViewUpdate::RevealIdentity,
        );
        assert_eq!(highlighted(&rows, &view), Some(id(3)));
        assert_eq!(view.scroll, SelectionScrollPolicy::RevealSelection);
    }

    #[test]
    fn pid_reuse_keeps_the_cached_creation_time() {
        let rows = [
            Row {
                identity: ProcIdentity::new(7, 100),
                name: "old.exe",
            },
            Row {
                identity: ProcIdentity::new(7, 200),
                name: "new.exe",
            },
        ];
        let view = plan(
            &rows,
            "new",
            Some(ProcIdentity::new(7, 100)),
            Some(ProcIdentity::new(7, 100)),
            None,
            ProcessViewUpdate::FilterChanged,
        );
        assert_eq!(visible_names(&rows, &view), ["new.exe"]);
        assert_eq!(highlighted(&rows, &view), Some(ProcIdentity::new(7, 200)));
        assert_eq!(view.identity_cache, Some(ProcIdentity::new(7, 100)));
    }

    #[test]
    fn neighbor_after_prefers_the_following_row() {
        let identities = [id(1), id(2), id(3)];
        assert_eq!(neighbor_after(&identities, id(1)), Some(id(2)));
        assert_eq!(neighbor_after(&identities, id(3)), Some(id(2)));
        assert_eq!(neighbor_after(&identities, id(9)), None);
        assert_eq!(neighbor_after(&[id(4)], id(4)), None);
    }
}
