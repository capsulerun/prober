const UNVISITED: usize = usize::MAX;

struct Frame {
    node: usize,
    successors: Vec<usize>,
    position: usize,
}

struct Tarjan {
    order: Vec<usize>,
    lowest: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    frames: Vec<Frame>,
    next_order: usize,
}

impl Tarjan {
    fn enter(&mut self, node: usize, successors: &mut impl FnMut(usize, &mut Vec<usize>)) {
        let mut node_successors = Vec::new();

        self.order[node] = self.next_order;
        self.lowest[node] = self.next_order;
        self.next_order += 1;
        self.on_stack[node] = true;
        self.stack.push(node);
        successors(node, &mut node_successors);
        self.frames.push(Frame {
            node,
            successors: node_successors,
            position: 0,
        });
    }

    fn component_rooted_at(&mut self, node: usize) -> Vec<usize> {
        let mut component = Vec::new();

        while let Some(member) = self.stack.pop() {
            self.on_stack[member] = false;
            component.push(member);

            if member == node {
                break;
            }
        }

        component
    }
}

pub(crate) fn strongly_connected_components(
    node_count: usize,
    mut successors: impl FnMut(usize, &mut Vec<usize>),
) -> Vec<Vec<usize>> {
    let mut tarjan = Tarjan {
        order: vec![UNVISITED; node_count],
        lowest: vec![UNVISITED; node_count],
        on_stack: vec![false; node_count],
        stack: Vec::new(),
        frames: Vec::new(),
        next_order: 0,
    };
    let mut components = Vec::new();

    for start in 0..node_count {
        if tarjan.order[start] != UNVISITED {
            continue;
        }

        tarjan.enter(start, &mut successors);

        while let Some(frame) = tarjan.frames.last_mut() {
            let node = frame.node;

            if let Some(&successor) = frame.successors.get(frame.position) {
                frame.position += 1;

                if tarjan.order[successor] == UNVISITED {
                    tarjan.enter(successor, &mut successors);
                } else if tarjan.on_stack[successor] {
                    tarjan.lowest[node] = tarjan.lowest[node].min(tarjan.order[successor]);
                }

                continue;
            }

            tarjan.frames.pop();

            if let Some(parent) = tarjan.frames.last() {
                let parent = parent.node;

                tarjan.lowest[parent] = tarjan.lowest[parent].min(tarjan.lowest[node]);
            }

            if tarjan.lowest[node] != tarjan.order[node] {
                continue;
            }

            let mut component = tarjan.component_rooted_at(node);

            if component.len() > 1 {
                component.sort_unstable();
                components.push(component);
            }
        }
    }

    components.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));

    components
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components_of(node_count: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
        strongly_connected_components(node_count, |node, successors| {
            successors.extend(
                edges
                    .iter()
                    .filter(|(from, _)| *from == node)
                    .map(|(_, to)| *to),
            );
        })
    }

    #[test]
    fn components_hold_mutually_reachable_nodes_largest_first() {
        let edges = [
            (0, 1),
            (1, 2),
            (2, 0),
            (2, 3),
            (3, 4),
            (4, 3),
            (5, 5),
            (6, 0),
        ];

        assert_eq!(components_of(7, &edges), [vec![0, 1, 2], vec![3, 4]]);
    }

    #[test]
    fn acyclic_graphs_have_no_component() {
        assert_eq!(
            components_of(4, &[(0, 1), (1, 2), (0, 2), (2, 3)]),
            Vec::<Vec<usize>>::new()
        );
    }

    #[test]
    fn long_chains_closing_into_one_cycle_do_not_overflow_the_stack() {
        let node_count = 200_000;
        let edges: Vec<(usize, usize)> = (0..node_count)
            .map(|node| (node, (node + 1) % node_count))
            .collect();
        let components = strongly_connected_components(node_count, |node, successors| {
            successors.push(edges[node].1);
        });

        assert_eq!(components.len(), 1);
        assert_eq!(components[0].len(), node_count);
    }
}
