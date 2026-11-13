//! Leiden community detection.
//!
//! Finds groups of nodes more densely connected to each other than to the
//! rest of a graph. On a code graph those groups come out as de facto
//! subsystems, which is what lets an agent be handed one part of a codebase
//! instead of all of it.
//!
//! ## Why Leiden rather than Louvain
//!
//! Louvain optimises the same quality function and is simpler, but it can
//! return a community that is **internally disconnected**: a node acting as
//! the only bridge inside a community gets moved out, and the orphaned
//! remainder keeps the label. A "subsystem" whose members do not reach each
//! other is worse than useless here -- it would send an agent context that is
//! not actually related.
//!
//! Leiden's fix is a refinement phase. After local moving produces a partition
//! P, each community of P is re-partitioned *from singletons, restricted to
//! that community*, giving P_refined. Aggregation then happens on P_refined
//! while the next round starts from P. A community can therefore be split, so
//! what survives is connected.
//!
//! ## What this is not
//!
//! Not an exact port of the reference implementation: there is no randomness
//! in the refinement (the published algorithm picks among well-connected
//! candidates at random), so this is deterministic. Determinism is worth more
//! here than the small quality gain -- the same codebase must produce the same
//! subsystems on every machine, or two agents would be told different things.

/// An undirected weighted graph over nodes `0..n`.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    n: usize,
    /// adjacency: node -> [(neighbour, weight)]
    adj: Vec<Vec<(usize, f64)>>,
    /// Sum of weights of edges incident to each node, self-loops counted
    /// twice -- the degree term the quality function needs.
    strength: Vec<f64>,
    /// Weight of a self-loop per node, kept because aggregation creates them.
    self_loops: Vec<f64>,
    total: f64,
}

impl Graph {
    pub fn with_nodes(n: usize) -> Self {
        Self {
            n,
            adj: vec![Vec::new(); n],
            strength: vec![0.0; n],
            self_loops: vec![0.0; n],
            total: 0.0,
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Add an undirected edge. Repeated pairs accumulate, so a caller may add
    /// one edge per observation rather than deduplicating first.
    pub fn add_edge(&mut self, a: usize, b: usize, w: f64) {
        if a >= self.n || b >= self.n || w <= 0.0 {
            return;
        }
        if a == b {
            self.self_loops[a] += w;
            self.strength[a] += 2.0 * w;
            self.total += w;
            return;
        }
        self.adj[a].push((b, w));
        self.adj[b].push((a, w));
        self.strength[a] += w;
        self.strength[b] += w;
        self.total += w;
    }

    fn neighbours(&self, v: usize) -> &[(usize, f64)] {
        &self.adj[v]
    }
}

/// `resolution` scales how much a community is penalised for its size: higher
/// gives more, smaller communities. 1.0 is standard modularity.
pub struct Options {
    pub resolution: f64,
    pub max_levels: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { resolution: 1.0, max_levels: 10 }
    }
}

/// Community index per node, renumbered from 0 and stable across runs.
pub fn communities(g: &Graph, opts: &Options) -> Vec<usize> {
    if g.n == 0 {
        return Vec::new();
    }
    if g.total <= 0.0 {
        // No edges: every node is its own community, which is the honest
        // answer rather than one community containing everything.
        return (0..g.n).collect();
    }

    // Membership of the ORIGINAL nodes, refined level by level.
    let mut membership: Vec<usize> = (0..g.n).collect();
    let mut graph = g.clone();
    // Which original nodes each current node stands for.
    let mut level_map: Vec<Vec<usize>> = (0..g.n).map(|v| vec![v]).collect();

    for _ in 0..opts.max_levels {
        // 1. Local moving: the Louvain step.
        let partition = local_move(&graph, opts.resolution);
        let distinct = count_distinct(&partition);
        if distinct == graph.n {
            // Nothing merged; no further level can help.
            break;
        }

        // 2. Refinement: re-partition each community from singletons,
        //    restricted to that community. This is what keeps a community
        //    internally connected.
        let refined = refine(&graph, &partition, opts.resolution);

        // Record the refined assignment against the original nodes.
        for (node, parts) in level_map.iter().enumerate() {
            for &orig in parts {
                membership[orig] = refined[node];
            }
        }

        // 3. Aggregate on the REFINED partition.
        let (next, next_map) = aggregate(&graph, &refined, &level_map);
        if next.n == graph.n {
            break;
        }
        graph = next;
        level_map = next_map;
    }

    renumber(&membership)
}

/// Move each node to the neighbouring community that most improves
/// modularity, repeating until nothing moves.
fn local_move(g: &Graph, resolution: f64) -> Vec<usize> {
    let m2 = 2.0 * g.total;
    let mut comm: Vec<usize> = (0..g.n).collect();
    let mut comm_strength: Vec<f64> = g.strength.clone();
    // A node's weight to each candidate community, reused to avoid
    // reallocating per node.
    let mut weights: Vec<f64> = vec![0.0; g.n];
    let mut touched: Vec<usize> = Vec::new();

    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 50 {
        improved = false;
        rounds += 1;
        for v in 0..g.n {
            let own = comm[v];
            comm_strength[own] -= g.strength[v];

            touched.clear();
            for &(u, w) in g.neighbours(v) {
                let c = comm[u];
                if weights[c] == 0.0 {
                    touched.push(c);
                }
                weights[c] += w;
            }
            if weights[own] == 0.0 {
                touched.push(own);
            }

            // Staying put is the baseline, so a tie never causes a move --
            // which is what keeps the result deterministic.
            let mut best = own;
            let mut best_gain =
                weights[own] - resolution * comm_strength[own] * g.strength[v] / m2;
            for &c in &touched {
                if c == own {
                    continue;
                }
                let gain = weights[c] - resolution * comm_strength[c] * g.strength[v] / m2;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best = c;
                }
            }
            for &c in &touched {
                weights[c] = 0.0;
            }

            comm_strength[best] += g.strength[v];
            if best != own {
                comm[v] = best;
                improved = true;
            }
        }
    }
    comm
}

/// Re-partition each community from singletons, allowing moves only within
/// that community. A community that is not well connected splits.
fn refine(g: &Graph, partition: &[usize], resolution: f64) -> Vec<usize> {
    let m2 = 2.0 * g.total;
    // Start from singletons: this is the difference from Louvain, which would
    // keep the community whole.
    let mut comm: Vec<usize> = (0..g.n).collect();
    let mut comm_strength: Vec<f64> = g.strength.clone();
    let mut weights: Vec<f64> = vec![0.0; g.n];
    let mut touched: Vec<usize> = Vec::new();

    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 50 {
        improved = false;
        rounds += 1;
        for v in 0..g.n {
            let own = comm[v];
            comm_strength[own] -= g.strength[v];
            touched.clear();
            for &(u, w) in g.neighbours(v) {
                // The restriction: only neighbours in the same community of
                // the coarse partition are candidates.
                if partition[u] != partition[v] {
                    continue;
                }
                let c = comm[u];
                if weights[c] == 0.0 {
                    touched.push(c);
                }
                weights[c] += w;
            }
            if weights[own] == 0.0 {
                touched.push(own);
            }
            let mut best = own;
            let mut best_gain =
                weights[own] - resolution * comm_strength[own] * g.strength[v] / m2;
            for &c in &touched {
                if c == own {
                    continue;
                }
                let gain = weights[c] - resolution * comm_strength[c] * g.strength[v] / m2;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best = c;
                }
            }
            for &c in &touched {
                weights[c] = 0.0;
            }
            comm_strength[best] += g.strength[v];
            if best != own {
                comm[v] = best;
                improved = true;
            }
        }
    }
    comm
}

/// Collapse each community into one node.
fn aggregate(
    g: &Graph,
    partition: &[usize],
    level_map: &[Vec<usize>],
) -> (Graph, Vec<Vec<usize>>) {
    let ids = renumber(partition);
    let k = count_distinct(&ids);
    let mut next = Graph::with_nodes(k);
    let mut map: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (node, parts) in level_map.iter().enumerate() {
        map[ids[node]].extend(parts.iter().copied());
    }
    // Self-loops carry the internal weight forward, or aggregation would lose
    // it and every level would look sparser than it is.
    for v in 0..g.n {
        if g.self_loops[v] > 0.0 {
            next.add_edge(ids[v], ids[v], g.self_loops[v]);
        }
        for &(u, w) in g.neighbours(v) {
            if u < v {
                continue; // each undirected edge once
            }
            next.add_edge(ids[v], ids[u], w);
        }
    }
    (next, map)
}

fn count_distinct(v: &[usize]) -> usize {
    let mut seen: Vec<usize> = v.to_vec();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

/// Renumber labels to 0.. in order of first appearance, so output does not
/// depend on internal ids.
fn renumber(v: &[usize]) -> Vec<usize> {
    let mut out = vec![0usize; v.len()];
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for (i, &c) in v.iter().enumerate() {
        match seen.iter().find(|(from, _)| *from == c) {
            Some((_, to)) => out[i] = *to,
            None => {
                let to = seen.len();
                seen.push((c, to));
                out[i] = to;
            }
        }
    }
    out
}

/// Modularity of a partition: for checking a result, and for tests.
pub fn modularity(g: &Graph, comm: &[usize], resolution: f64) -> f64 {
    if g.total <= 0.0 {
        return 0.0;
    }
    let m2 = 2.0 * g.total;
    let k = count_distinct(comm);
    let mut internal = vec![0.0; k];
    let mut strength = vec![0.0; k];
    let ids = renumber(comm);
    for v in 0..g.n {
        strength[ids[v]] += g.strength[v];
        internal[ids[v]] += g.self_loops[v] * 2.0;
        for &(u, w) in g.neighbours(v) {
            if ids[u] == ids[v] {
                internal[ids[v]] += w;
            }
        }
    }
    (0..k)
        .map(|c| internal[c] / m2 - resolution * (strength[c] / m2).powi(2))
        .sum()
}

/// Is every community internally connected? The property Leiden guarantees
/// and Louvain does not.
pub fn all_communities_connected(g: &Graph, comm: &[usize]) -> bool {
    let k = count_distinct(comm);
    let ids = renumber(comm);
    for c in 0..k {
        let members: Vec<usize> = (0..g.n).filter(|&v| ids[v] == c).collect();
        if members.len() <= 1 {
            continue;
        }
        let mut seen = vec![false; g.n];
        let mut stack = vec![members[0]];
        seen[members[0]] = true;
        let mut count = 1;
        while let Some(v) = stack.pop() {
            for &(u, _) in g.neighbours(v) {
                if ids[u] == c && !seen[u] {
                    seen[u] = true;
                    count += 1;
                    stack.push(u);
                }
            }
        }
        if count != members.len() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clique(g: &mut Graph, nodes: &[usize]) {
        for (i, &a) in nodes.iter().enumerate() {
            for &b in &nodes[i + 1..] {
                g.add_edge(a, b, 1.0);
            }
        }
    }

    #[test]
    fn an_empty_graph_has_no_communities() {
        assert!(communities(&Graph::with_nodes(0), &Options::default()).is_empty());
    }

    #[test]
    fn nodes_with_no_edges_are_each_their_own() {
        // Not one big community: there is no evidence they belong together.
        let got = communities(&Graph::with_nodes(4), &Options::default());
        assert_eq!(count_distinct(&got), 4);
    }

    #[test]
    fn two_cliques_joined_by_one_edge_are_two_communities() {
        // The canonical case: the bridge must not merge them.
        let mut g = Graph::with_nodes(10);
        clique(&mut g, &[0, 1, 2, 3, 4]);
        clique(&mut g, &[5, 6, 7, 8, 9]);
        g.add_edge(4, 5, 1.0);
        let got = communities(&g, &Options::default());
        assert_eq!(count_distinct(&got), 2, "{got:?}");
        assert_eq!(got[0], got[1], "the first clique stays together: {got:?}");
        assert_eq!(got[0], got[4]);
        assert_eq!(got[5], got[9], "and so does the second");
        assert_ne!(got[0], got[5], "but they are not the same community");
    }

    #[test]
    fn a_single_clique_is_one_community() {
        let mut g = Graph::with_nodes(6);
        clique(&mut g, &[0, 1, 2, 3, 4, 5]);
        assert_eq!(count_distinct(&communities(&g, &Options::default())), 1);
    }

    #[test]
    fn three_cliques_in_a_chain_are_three_communities() {
        let mut g = Graph::with_nodes(12);
        clique(&mut g, &[0, 1, 2, 3]);
        clique(&mut g, &[4, 5, 6, 7]);
        clique(&mut g, &[8, 9, 10, 11]);
        g.add_edge(3, 4, 1.0);
        g.add_edge(7, 8, 1.0);
        let got = communities(&g, &Options::default());
        assert_eq!(count_distinct(&got), 3, "{got:?}");
    }

    #[test]
    fn every_community_is_internally_connected() {
        // The property that makes this Leiden and not Louvain. A community
        // whose members cannot reach each other is not a subsystem, and
        // handing one to an agent as context would be actively misleading.
        let mut g = Graph::with_nodes(14);
        clique(&mut g, &[0, 1, 2, 3, 4]);
        clique(&mut g, &[5, 6, 7, 8, 9]);
        // A node bridging both, plus a weak tail: the shape that trips Louvain.
        g.add_edge(10, 0, 1.0);
        g.add_edge(10, 5, 1.0);
        g.add_edge(11, 10, 1.0);
        g.add_edge(12, 11, 1.0);
        g.add_edge(13, 12, 1.0);
        let got = communities(&g, &Options::default());
        assert!(all_communities_connected(&g, &got), "{got:?}");
    }

    #[test]
    fn the_result_is_deterministic() {
        // Two agents on two machines must be told the same subsystems.
        let mut g = Graph::with_nodes(20);
        clique(&mut g, &[0, 1, 2, 3, 4, 5]);
        clique(&mut g, &[6, 7, 8, 9, 10]);
        clique(&mut g, &[11, 12, 13, 14, 15]);
        for v in 16..20 {
            g.add_edge(v, v - 16, 1.0);
        }
        g.add_edge(5, 6, 1.0);
        g.add_edge(10, 11, 1.0);
        let first = communities(&g, &Options::default());
        for _ in 0..5 {
            assert_eq!(communities(&g, &Options::default()), first);
        }
    }

    #[test]
    fn partitioning_beats_putting_everything_in_one_community() {
        let mut g = Graph::with_nodes(10);
        clique(&mut g, &[0, 1, 2, 3, 4]);
        clique(&mut g, &[5, 6, 7, 8, 9]);
        g.add_edge(4, 5, 1.0);
        let got = communities(&g, &Options::default());
        let one = vec![0usize; 10];
        assert!(
            modularity(&g, &got, 1.0) > modularity(&g, &one, 1.0),
            "found {:.4} vs single {:.4}",
            modularity(&g, &got, 1.0),
            modularity(&g, &one, 1.0)
        );
    }

    #[test]
    fn a_higher_resolution_does_not_give_fewer_communities() {
        let mut g = Graph::with_nodes(16);
        clique(&mut g, &[0, 1, 2, 3, 4, 5, 6, 7]);
        clique(&mut g, &[8, 9, 10, 11, 12, 13, 14, 15]);
        g.add_edge(7, 8, 1.0);
        let coarse = count_distinct(&communities(&g, &Options { resolution: 0.5, max_levels: 10 }));
        let fine = count_distinct(&communities(&g, &Options { resolution: 3.0, max_levels: 10 }));
        assert!(fine >= coarse, "coarse {coarse}, fine {fine}");
    }

    #[test]
    fn weights_decide_where_a_shared_node_goes() {
        let mut g = Graph::with_nodes(7);
        clique(&mut g, &[0, 1, 2]);
        clique(&mut g, &[3, 4, 5]);
        // Node 6 leans heavily on the first group.
        g.add_edge(6, 0, 10.0);
        g.add_edge(6, 3, 1.0);
        let got = communities(&g, &Options::default());
        assert_eq!(got[6], got[0], "{got:?}");
    }

    #[test]
    fn self_loops_and_repeated_edges_are_accumulated_not_lost() {
        let mut g = Graph::with_nodes(4);
        g.add_edge(0, 1, 1.0);
        g.add_edge(0, 1, 1.0); // same pair twice: one edge of weight 2
        g.add_edge(2, 3, 2.0);
        g.add_edge(0, 0, 5.0); // a self-loop, as aggregation produces
        let got = communities(&g, &Options::default());
        assert_eq!(got[0], got[1]);
        assert_eq!(got[2], got[3]);
        assert_ne!(got[0], got[2]);
    }

    #[test]
    fn a_disconnected_graph_keeps_its_parts_apart() {
        let mut g = Graph::with_nodes(8);
        clique(&mut g, &[0, 1, 2, 3]);
        clique(&mut g, &[4, 5, 6, 7]);
        let got = communities(&g, &Options::default());
        assert_ne!(got[0], got[4], "nothing joins them: {got:?}");
        assert!(all_communities_connected(&g, &got));
    }

    #[test]
    fn nonsense_edges_are_ignored_rather_than_panicking() {
        let mut g = Graph::with_nodes(2);
        g.add_edge(0, 5, 1.0); // out of range
        g.add_edge(0, 1, 0.0); // zero weight
        g.add_edge(0, 1, -3.0); // negative
        assert_eq!(count_distinct(&communities(&g, &Options::default())), 2);
    }
}
