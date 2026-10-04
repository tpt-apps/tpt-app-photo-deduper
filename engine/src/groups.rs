//! Duplicate grouping over [`PhotoRecord`]s: exact (content hash) and
//! perceptual near-duplicate (dHash + Hamming threshold) via union-find.

use crate::{hamming_distance, PhotoRecord};
use std::collections::HashMap;

/// One byte-identical cluster. `hash` is the shared SHA-256; `member_ids`
/// are the records' ids sorted by size descending then name (the UI's
/// default keeper is the first member).
#[derive(Debug, Clone)]
pub struct DupGroup {
    pub hash: [u8; 32],
    pub member_ids: Vec<u32>,
}

/// One perceptual cluster from [`group_near`]: identical-content files and
/// visually-similar (Hamming distance ≤ threshold) files merged into the
/// same component. `max_distance` is 0 for pure byte-identical groups — the
/// UI uses that to label the group "identical" vs "near-duplicate".
#[derive(Debug, Clone)]
pub struct NearGroup {
    pub member_ids: Vec<u32>,
    pub max_distance: u32,
}

/// Read-only view over the records shared by the grouping functions, so
/// lookups stay O(1) instead of per-group linear scans.
struct Registry<'a> {
    by_id: HashMap<u32, &'a PhotoRecord>,
}

impl<'a> Registry<'a> {
    fn new(records: &'a [PhotoRecord]) -> Self {
        Self { by_id: records.iter().map(|r| (r.id, r)).collect() }
    }

    fn size(&self, id: u32) -> u64 {
        self.by_id.get(&id).map_or(0, |r| r.size)
    }

    fn name(&self, id: u32) -> &str {
        self.by_id.get(&id).map_or("", |r| r.name.as_str())
    }

    /// Wasted bytes of a duplicate group: everything above the largest
    /// member (keeping one copy frees the rest).
    fn wasted(&self, ids: &[u32]) -> u64 {
        let total: u64 = ids.iter().map(|id| self.size(*id)).sum();
        let max = ids.iter().map(|id| self.size(*id)).max().unwrap_or(0);
        total.saturating_sub(max)
    }

    /// Sorts member ids by size descending then name ascending, so the UI's
    /// default keeper (first member) is the highest-quality / most stable
    /// pick.
    fn sort_members(&self, ids: &mut [u32]) {
        ids.sort_by(|a, b| {
            self.size(*b).cmp(&self.size(*a)).then_with(|| self.name(*a).cmp(self.name(*b)))
        });
    }
}

/// Groups records whose SHA-256 content hashes collide. Only clusters with
/// two or more members are returned, largest wasted bytes first.
pub fn group_exact(records: &[PhotoRecord]) -> Vec<DupGroup> {
    let registry = Registry::new(records);
    let mut by_hash: HashMap<[u8; 32], Vec<u32>> = HashMap::new();
    for record in records {
        by_hash.entry(record.hash).or_default().push(record.id);
    }
    let mut groups: Vec<(DupGroup, u64)> = by_hash
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(hash, mut ids)| {
            registry.sort_members(&mut ids);
            let wasted = registry.wasted(&ids);
            (DupGroup { hash, member_ids: ids }, wasted)
        })
        .collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    groups.into_iter().map(|(group, _)| group).collect()
}

/// Groups records into near-duplicate clusters (the Pro grouping):
///
/// 1. every pair sharing an exact content hash is unioned — byte-identical
///    files belong together whether or not they were decodable;
/// 2. every pair of records carrying a dHash within `threshold` Hamming
///    bits of each other is unioned.
///
/// Components with a single member are dropped. Groups come back largest
/// wasted bytes first. Use [`DEFAULT_NEAR_THRESHOLD`] (8) unless the user
/// explicitly asked for looser/stricter matching.
pub fn group_near(records: &[PhotoRecord], threshold: u32) -> Vec<NearGroup> {
    let registry = Registry::new(records);

    // Ids are not guaranteed dense or ordered (deletions), so union-find
    // runs over record positions.
    let mut parent: Vec<usize> = (0..records.len()).collect();
    fn find(parent: &mut [usize], start: usize) -> usize {
        let mut root = start;
        while parent[root] != root {
            root = parent[root];
        }
        let mut node = start;
        while parent[node] != node {
            let next = parent[node];
            parent[node] = root;
            node = next;
        }
        root
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[rb] = ra;
        }
    }

    // 1. Byte-identical files merge unconditionally.
    let mut first_by_hash: HashMap<[u8; 32], usize> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        match first_by_hash.get(&record.hash) {
            Some(&first) => union(&mut parent, first, i),
            None => {
                first_by_hash.insert(record.hash, i);
            }
        }
    }

    // 2. Perceptual neighbours among the records that carry a dHash. The
    //    quadratic pair sweep is fine at culling-session scale (10k decodable
    //    photos ≈ 5·10⁷ `count_ones` — well under a second on the desktop
    //    host, and the free edition never calls this).
    let hashed: Vec<(usize, u64)> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.perceptual.map(|hash| (i, hash)))
        .collect();
    for a in 0..hashed.len() {
        for b in (a + 1)..hashed.len() {
            if hamming_distance(hashed[a].1, hashed[b].1) <= threshold {
                union(&mut parent, hashed[a].0, hashed[b].0);
            }
        }
    }

    // Collect components with more than one member.
    let mut components: HashMap<usize, Vec<u32>> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        components.entry(find(&mut parent, i)).or_default().push(record.id);
    }

    let mut groups: Vec<(NearGroup, u64)> = components
        .into_values()
        .filter(|ids| ids.len() > 1)
        .map(|ids| {
            // Distances are measured against the first hashed member (the
            // anchor), in stable record order, before the display sort.
            let anchor = ids
                .iter()
                .filter_map(|id| registry.by_id.get(id).and_then(|r| r.perceptual))
                .next();
            let max_distance = match anchor {
                Some(anchor) => ids
                    .iter()
                    .filter_map(|id| registry.by_id.get(id).and_then(|r| r.perceptual))
                    .map(|hash| hamming_distance(anchor, hash))
                    .max()
                    .unwrap_or(0),
                None => 0,
            };
            let wasted = registry.wasted(&ids);
            let mut member_ids = ids;
            registry.sort_members(&mut member_ids);
            (NearGroup { member_ids, max_distance }, wasted)
        })
        .collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    groups.into_iter().map(|(group, _)| group).collect()
}
