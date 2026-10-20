// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程树布局
//
//   文件:       src/pages/processes/tree.rs
//
//   日期:       2026年10月20日
//   环境:       Linux 6.12.94+，rustc 1.99.1
//   作者:       Cursor Grok 4.7
// --------------------------------------------------------------------------

//! Parent links and the visible row order for the Processes page.
//!
//! The background worker resolves every parent from the same committed
//! snapshot that produced the rows. A link is a `ProcIdentity` pair: a reused
//! PID is not a parent. This module never opens a process. The UI thread only
//! consumes the resolved edges when it lays out, expands, collapses, or paints.

use std::collections::{HashMap, HashSet};

use crate::system::process_identity::ProcIdentity;

/// One process in a single snapshot, before its parent identity is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessLink {
    pub(super) identity: ProcIdentity,
    /// Parent PID from that same snapshot. `None` when the snapshot had no
    /// usable parent PID.
    pub(super) parent_pid: Option<u32>,
}

/// One visible list row. `entry_index` addresses the page's entry vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TreeRow {
    pub(super) entry_index: usize,
    pub(super) identity: ProcIdentity,
    pub(super) depth: u16,
    pub(super) has_children: bool,
    pub(super) expanded: bool,
}

const MAX_DISPLAY_DEPTH: u16 = 32;

/// Resolves a parent identity for each link.
///
/// A row stays a root when it is unverified, its parent PID is absent, the
/// parent is unverified, or the PID is ambiguous. When several snapshot rows
/// share the parent PID, the edge is kept only if exactly one of them was
/// created at or before the child. A newer process that reused the PID does
/// not adopt the older child.
pub(super) fn resolve_parent_identities(links: &[ProcessLink]) -> Vec<Option<ProcIdentity>> {
    let mut by_pid: HashMap<u32, Vec<ProcIdentity>> = HashMap::new();
    for link in links {
        if link.identity.is_verified() {
            by_pid
                .entry(link.identity.pid)
                .or_default()
                .push(link.identity);
        }
    }

    links
        .iter()
        .map(|link| resolve_parent_identity(*link, &by_pid))
        .collect()
}

fn resolve_parent_identity(
    link: ProcessLink,
    by_pid: &HashMap<u32, Vec<ProcIdentity>>,
) -> Option<ProcIdentity> {
    if !link.identity.is_verified() {
        return None;
    }
    let parent_pid = link.parent_pid?;
    let candidates = by_pid.get(&parent_pid)?;
    let mut matched = None;
    for candidate in candidates {
        if *candidate == link.identity
            || candidate.creation_time_100ns > link.identity.creation_time_100ns
        {
            continue;
        }
        if matched.is_some() {
            return None;
        }
        matched = Some(*candidate);
    }
    matched
}

/// Builds the visible row order.
///
/// `identities` and `parents` are parallel and already in sibling sort order.
/// In list mode every row is a depth-zero leaf. In tree mode a missing parent
/// is a root, and a cycle is broken instead of dropping a row. Children of a
/// collapsed process are omitted; every other row appears once.
///
/// `collapsed` holds identities the user folded. A parent that is absent from
/// that set stays open, including a parent that appears on a later refresh.
pub(super) fn layout_process_rows(
    identities: &[ProcIdentity],
    parents: &[Option<ProcIdentity>],
    collapsed: &HashSet<ProcIdentity>,
    tree_mode: bool,
) -> Vec<TreeRow> {
    if identities.len() != parents.len() || !tree_mode {
        return flat_rows(identities);
    }

    let mut first_index = HashMap::with_capacity(identities.len());
    for (index, identity) in identities.iter().copied().enumerate() {
        first_index.entry(identity).or_insert(index);
    }

    let mut parent_of = vec![None; identities.len()];
    for (index, parent) in parents.iter().copied().enumerate() {
        let Some(parent) = parent else {
            continue;
        };
        if parent == identities[index] {
            continue;
        }
        let Some(&parent_index) = first_index.get(&parent) else {
            continue;
        };
        if parent_index != index {
            parent_of[index] = Some(parent_index);
        }
    }
    detach_cycles(&mut parent_of);

    let mut children = vec![Vec::new(); identities.len()];
    let mut is_child = vec![false; identities.len()];
    for (index, parent_index) in parent_of.iter().copied().enumerate() {
        if let Some(parent_index) = parent_index {
            children[parent_index].push(index);
            is_child[index] = true;
        }
    }

    let mut rows = Vec::with_capacity(identities.len());
    let mut visited = vec![false; identities.len()];
    let mut stack = Vec::new();
    for index in (0..identities.len()).rev() {
        if !is_child[index] {
            stack.push((index, 0u16));
        }
    }

    while let Some((index, depth)) = stack.pop() {
        if visited[index] {
            continue;
        }
        visited[index] = true;
        let has_children = !children[index].is_empty();
        let is_expanded = has_children && !collapsed.contains(&identities[index]);
        rows.push(TreeRow {
            entry_index: index,
            identity: identities[index],
            depth,
            has_children,
            expanded: is_expanded,
        });
        if is_expanded {
            let child_depth = depth.saturating_add(1);
            for &child in children[index].iter().rev() {
                stack.push((child, child_depth));
            }
        } else if has_children {
            hide_descendants(index, &children, &mut visited);
        }
    }

    for index in 0..identities.len() {
        if !visited[index] {
            rows.push(TreeRow {
                entry_index: index,
                identity: identities[index],
                depth: 0,
                has_children: false,
                expanded: false,
            });
        }
    }
    rows
}

fn flat_rows(identities: &[ProcIdentity]) -> Vec<TreeRow> {
    identities
        .iter()
        .copied()
        .enumerate()
        .map(|(entry_index, identity)| TreeRow {
            entry_index,
            identity,
            depth: 0,
            has_children: false,
            expanded: false,
        })
        .collect()
}

fn hide_descendants(root: usize, children: &[Vec<usize>], visited: &mut [bool]) {
    let mut pending = Vec::new();
    pending.extend_from_slice(&children[root]);
    while let Some(index) = pending.pop() {
        if visited[index] {
            continue;
        }
        visited[index] = true;
        pending.extend_from_slice(&children[index]);
    }
}

/// Clears the edge that closes a cycle so every node remains reachable from a root.
fn detach_cycles(parent_of: &mut [Option<usize>]) {
    let mut stamp = vec![0u32; parent_of.len()];
    let mut generation = 1u32;
    for start in 0..parent_of.len() {
        generation = generation.wrapping_add(1);
        if generation == 0 {
            stamp.fill(0);
            generation = 1;
        }
        let mut cursor = Some(start);
        let mut previous = None;
        while let Some(node) = cursor {
            if stamp[node] == generation {
                if let Some(previous) = previous {
                    parent_of[previous] = None;
                }
                break;
            }
            stamp[node] = generation;
            previous = Some(node);
            cursor = parent_of[node];
        }
    }
}

pub(super) fn retain_live_identities(
    identities: &mut HashSet<ProcIdentity>,
    live: &HashSet<ProcIdentity>,
) {
    identities.retain(|identity| live.contains(identity));
}

/// Ancestors that must be expanded before `target` can appear. The target itself
/// is not included.
pub(super) fn expansion_ancestors(
    identities: &[ProcIdentity],
    parents: &[Option<ProcIdentity>],
    target: ProcIdentity,
) -> Vec<ProcIdentity> {
    let parent_of = parent_lookup(identities, parents);
    let mut ancestors = Vec::new();
    let mut cursor = parent_of.get(&target).copied();
    let mut seen = HashSet::new();
    while let Some(identity) = cursor {
        if !seen.insert(identity) {
            break;
        }
        ancestors.push(identity);
        cursor = parent_of.get(&identity).copied();
    }
    ancestors.reverse();
    ancestors
}

/// The nearest visible row for a selection whose own row is collapsed away.
pub(super) fn nearest_visible_identity(
    identity: ProcIdentity,
    identities: &[ProcIdentity],
    parents: &[Option<ProcIdentity>],
    visible: &HashSet<ProcIdentity>,
) -> Option<ProcIdentity> {
    let parent_of = parent_lookup(identities, parents);
    let mut cursor = Some(identity);
    let mut seen = HashSet::new();
    while let Some(identity) = cursor {
        if visible.contains(&identity) {
            return Some(identity);
        }
        if !seen.insert(identity) {
            return None;
        }
        cursor = parent_of.get(&identity).copied();
    }
    None
}

fn parent_lookup(
    identities: &[ProcIdentity],
    parents: &[Option<ProcIdentity>],
) -> HashMap<ProcIdentity, ProcIdentity> {
    let mut parent_of = HashMap::new();
    for (identity, parent) in identities.iter().copied().zip(parents.iter().copied()) {
        if let Some(parent) = parent
            && parent != identity
        {
            parent_of.entry(identity).or_insert(parent);
        }
    }
    parent_of
}

pub(super) fn same_tree_presentation(left: &[TreeRow], right: &[TreeRow]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.identity == right.identity
                && left.depth == right.depth
                && left.has_children == right.has_children
                && left.expanded == right.expanded
        })
}

/// Prefix painted in front of the image name. Empty when the row needs no marker.
pub(super) fn tree_image_prefix(row: &TreeRow) -> String {
    if !row.has_children && row.depth == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    for _ in 0..usize::from(row.depth.min(MAX_DISPLAY_DEPTH)) {
        prefix.push_str("  ");
    }
    prefix.push_str(if !row.has_children {
        "  "
    } else if row.expanded {
        "- "
    } else {
        "+ "
    });
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(pid: u32, created: u64, parent_pid: Option<u32>) -> ProcessLink {
        ProcessLink {
            identity: ProcIdentity::new(pid, created),
            parent_pid,
        }
    }

    fn identities_of(rows: &[TreeRow]) -> Vec<u32> {
        rows.iter().map(|row| row.identity.pid).collect()
    }

    #[test]
    fn missing_parent_and_unverified_rows_stay_roots() {
        let links = [
            link(4, 50, Some(0)),
            link(20, 100, Some(99)),
            ProcessLink {
                identity: ProcIdentity::pid_only(7),
                parent_pid: Some(4),
            },
        ];
        let parents = resolve_parent_identities(&links);
        assert_eq!(parents, vec![None, None, None]);
    }

    #[test]
    fn reused_parent_pid_does_not_adopt_an_older_child() {
        let links = [
            link(10, 200, None),
            link(20, 100, Some(10)),
            link(30, 250, Some(10)),
        ];
        let parents = resolve_parent_identities(&links);
        assert_eq!(parents[0], None);
        assert_eq!(parents[1], None);
        assert_eq!(parents[2], Some(ProcIdentity::new(10, 200)));
    }

    #[test]
    fn exactly_one_older_pid_owner_is_the_parent() {
        let links = [
            link(10, 40, None),
            link(10, 180, None),
            link(20, 100, Some(10)),
        ];
        let parents = resolve_parent_identities(&links);
        assert_eq!(parents[2], Some(ProcIdentity::new(10, 40)));
    }

    #[test]
    fn two_older_owners_of_the_same_pid_leave_the_child_a_root() {
        let links = [
            link(10, 40, None),
            link(10, 50, None),
            link(20, 100, Some(10)),
        ];
        let parents = resolve_parent_identities(&links);
        assert_eq!(parents[2], None);
    }

    #[test]
    fn a_dropped_row_does_not_remove_its_siblings() {
        let present = [link(4, 50, None), link(30, 90, Some(99))];
        let parents = resolve_parent_identities(&present);
        let identities = [present[0].identity, present[1].identity];
        let rows = layout_process_rows(&identities, &parents, &HashSet::new(), true);
        assert_eq!(identities_of(&rows), vec![4, 30]);
        assert!(rows.iter().all(|row| row.depth == 0));
    }

    #[test]
    fn valid_parent_groups_children_and_collapse_hides_them() {
        let links = [
            link(8, 80, Some(4)),
            link(4, 50, None),
            link(9, 70, Some(4)),
        ];
        let parents = resolve_parent_identities(&links);
        let identities: Vec<_> = links.iter().map(|link| link.identity).collect();
        let mut collapsed_parents = HashSet::new();
        collapsed_parents.insert(ProcIdentity::new(4, 50));
        let collapsed = layout_process_rows(&identities, &parents, &collapsed_parents, true);
        assert_eq!(identities_of(&collapsed), vec![4]);
        assert!(
            collapsed
                .iter()
                .find(|row| row.identity.pid == 4)
                .unwrap()
                .has_children
        );
        assert!(
            !collapsed
                .iter()
                .find(|row| row.identity.pid == 4)
                .unwrap()
                .expanded
        );

        let shown = layout_process_rows(&identities, &parents, &HashSet::new(), true);
        assert_eq!(identities_of(&shown), vec![4, 8, 9]);
        assert_eq!(shown[0].depth, 0);
        assert!(shown[0].expanded);
        assert_eq!(shown[1].depth, 1);
        assert_eq!(shown[2].depth, 1);
        assert_eq!(tree_image_prefix(&shown[0]), "- ");
        assert_eq!(tree_image_prefix(&shown[1]), "    ");
    }

    #[test]
    fn list_mode_ignores_parent_links() {
        let identities = [ProcIdentity::new(8, 80), ProcIdentity::new(4, 50)];
        let parents = [Some(identities[1]), None];
        let rows = layout_process_rows(&identities, &parents, &HashSet::new(), false);
        assert_eq!(identities_of(&rows), vec![8, 4]);
        assert!(rows.iter().all(|row| row.depth == 0 && !row.has_children));
        assert_eq!(tree_image_prefix(&rows[0]), "");
    }

    #[test]
    fn a_cycle_keeps_every_row() {
        let identities = [ProcIdentity::new(1, 10), ProcIdentity::new(2, 20)];
        let parents = [Some(identities[1]), Some(identities[0])];
        let rows = layout_process_rows(&identities, &parents, &HashSet::new(), true);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .map(|row| row.identity.pid)
                .collect::<HashSet<_>>(),
            HashSet::from([1, 2])
        );
    }

    #[test]
    fn refresh_drops_expansion_for_an_exited_process_only() {
        let parent = ProcIdentity::new(4, 50);
        let child = ProcIdentity::new(8, 80);
        let mut collapsed = HashSet::from([parent, child]);
        retain_live_identities(&mut collapsed, &HashSet::from([child]));
        assert_eq!(collapsed, HashSet::from([child]));
    }

    #[test]
    fn ancestors_and_visible_fallback_follow_creation_checked_parents() {
        let identities = [
            ProcIdentity::new(4, 50),
            ProcIdentity::new(8, 80),
            ProcIdentity::new(12, 120),
        ];
        let parents = [None, Some(identities[0]), Some(identities[1])];
        assert_eq!(
            expansion_ancestors(&identities, &parents, identities[2]),
            vec![identities[0], identities[1]]
        );
        let visible = HashSet::from([identities[0]]);
        assert_eq!(
            nearest_visible_identity(identities[2], &identities, &parents, &visible),
            Some(identities[0])
        );
        assert_eq!(
            nearest_visible_identity(identities[1], &identities, &parents, &visible),
            Some(identities[0])
        );
    }

    #[test]
    fn collapsed_marker_uses_a_plus_and_roots_have_no_prefix() {
        let collapsed = TreeRow {
            entry_index: 0,
            identity: ProcIdentity::new(4, 50),
            depth: 0,
            has_children: true,
            expanded: false,
        };
        assert_eq!(tree_image_prefix(&collapsed), "+ ");
        let root = TreeRow {
            has_children: false,
            expanded: false,
            ..collapsed
        };
        assert_eq!(tree_image_prefix(&root), "");
    }
}
