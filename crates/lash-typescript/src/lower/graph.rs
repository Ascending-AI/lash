/// The shortest cycle through `start`, as `start -> … -> start`.
pub(super) fn shortest_cycle_through(edges: &[Vec<usize>], start: usize) -> Vec<usize> {
    let mut parent = vec![None; edges.len()];
    let mut queue = std::collections::VecDeque::from([start]);
    let mut seen = vec![false; edges.len()];
    seen[start] = true;
    while let Some(node) = queue.pop_front() {
        for target in &edges[node] {
            if *target == start {
                let mut path = vec![start];
                let mut step = Some(node);
                while let Some(current) = step {
                    path.push(current);
                    step = parent[current];
                }
                path.reverse();
                path.push(start);
                path.dedup();
                return path;
            }
            if !seen[*target] {
                seen[*target] = true;
                parent[*target] = Some(node);
                queue.push_back(*target);
            }
        }
    }
    vec![start]
}

/// Kosaraju's algorithm over the capture graph, iterative so that a deeply
/// chained set of declarations cannot exhaust the native stack.
pub(super) fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut reversed = vec![Vec::new(); edges.len()];
    for (from, targets) in edges.iter().enumerate() {
        for to in targets {
            reversed[*to].push(from);
        }
    }

    let mut order = Vec::with_capacity(edges.len());
    let mut visited = vec![false; edges.len()];
    for start in 0..edges.len() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.pop() {
            match edges[node].get(next) {
                Some(target) => {
                    stack.push((node, next + 1));
                    if !visited[*target] {
                        visited[*target] = true;
                        stack.push((*target, 0));
                    }
                }
                None => order.push(node),
            }
        }
    }

    let mut components = Vec::new();
    let mut assigned = vec![false; edges.len()];
    for start in order.into_iter().rev() {
        if assigned[start] {
            continue;
        }
        assigned[start] = true;
        let mut component = vec![start];
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for target in &reversed[node] {
                if !assigned[*target] {
                    assigned[*target] = true;
                    component.push(*target);
                    stack.push(*target);
                }
            }
        }
        component.sort_unstable();
        components.push(component);
    }
    components
}
