use crate::FileId;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug)]
pub struct FileDependencyRelation<'a> {
    dependencies: &'a HashMap<FileId, HashSet<FileId>>,
}

impl<'a> FileDependencyRelation<'a> {
    pub fn new(dependencies: &'a HashMap<FileId, HashSet<FileId>>) -> Self {
        Self { dependencies }
    }

    pub fn get_best_analysis_order(
        &self,
        file_ids: &[FileId],
        metas: &HashSet<FileId>,
    ) -> Vec<FileId> {
        self.get_analysis_levels(file_ids, metas)
            .into_iter()
            .flatten()
            .collect()
    }

    /// [`Self::get_best_analysis_order`] grouped into dependency levels: no
    /// file depends on a same-level sibling; flattening reproduces the flat
    /// order exactly. Cycle leftovers become single-file levels.
    pub fn get_analysis_levels(
        &self,
        file_ids: &[FileId],
        metas: &HashSet<FileId>,
    ) -> Vec<Vec<FileId>> {
        let n = file_ids.len();
        if n < 2 {
            return file_ids.iter().map(|&f| vec![f]).collect();
        }

        let file_to_idx: HashMap<FileId, usize> =
            file_ids.iter().enumerate().map(|(i, &f)| (f, i)).collect();

        let mut in_degree = vec![0usize; n];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];

        for (idx, &file_id) in file_ids.iter().enumerate() {
            if let Some(deps) = self.dependencies.get(&file_id) {
                for &dep in deps {
                    if let Some(&dep_idx) = file_to_idx.get(&dep) {
                        adjacency[dep_idx].push(idx);
                        in_degree[idx] += 1;
                    }
                }
            }
        }
        let mut levels: Vec<Vec<FileId>> = Vec::new();
        let mut node_level = vec![0usize; n];
        let mut queue = VecDeque::with_capacity(n);
        let mut popped = 0usize;

        // 入度为0的节点，按优先级排序：meta文件优先，然后按FileId排序
        let mut zero_in_degree: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
        zero_in_degree.sort_by(|&a, &b| {
            let a_is_meta = metas.contains(&file_ids[a]);
            let b_is_meta = metas.contains(&file_ids[b]);
            // meta文件优先（true > false，所以反过来比较）
            match (b_is_meta, a_is_meta) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => file_ids[a].cmp(&file_ids[b]),
            }
        });

        for idx in zero_in_degree {
            queue.push_back(idx);
        }

        while let Some(idx) = queue.pop_front() {
            let level = node_level[idx];
            if levels.len() == level {
                levels.push(Vec::new());
            }
            levels[level].push(file_ids[idx]);
            popped += 1;

            // 收集新的入度为0的节点
            let mut new_zero: Vec<usize> = Vec::new();
            for &neighbor in &adjacency[idx] {
                in_degree[neighbor] -= 1;
                if in_degree[neighbor] == 0 {
                    // FIFO pops breadth-first, so `idx` is the deepest dependency.
                    node_level[neighbor] = level + 1;
                    new_zero.push(neighbor);
                }
            }

            // 同样按优先级排序后加入队列
            if new_zero.len() > 1 {
                new_zero.sort_by(|&a, &b| {
                    let a_is_meta = metas.contains(&file_ids[a]);
                    let b_is_meta = metas.contains(&file_ids[b]);
                    match (b_is_meta, a_is_meta) {
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        _ => file_ids[a].cmp(&file_ids[b]),
                    }
                });
            }
            for neighbor in new_zero {
                queue.push_back(neighbor);
            }
        }

        // 处理循环依赖
        if popped < n {
            for (idx, &deg) in in_degree.iter().enumerate() {
                if deg > 0 {
                    // One file per level: a cycle has no safe concurrent order.
                    levels.push(vec![file_ids[idx]]);
                }
            }
        }

        levels
    }

    /// Get all direct and indirect dependencies for the file list
    pub fn collect_file_dependents(&self, file_ids: Vec<FileId>) -> Vec<FileId> {
        let mut reverse_map: HashMap<FileId, Vec<FileId>> = HashMap::new();
        for (&fid, deps) in self.dependencies.iter() {
            for &dep in deps {
                reverse_map.entry(dep).or_default().push(fid);
            }
        }
        let mut result = HashSet::new();
        let mut queue = VecDeque::new();
        for file_id in file_ids {
            queue.push_back(file_id);
        }
        while let Some(file_id) = queue.pop_front() {
            if let Some(dependents) = reverse_map.get(&file_id) {
                for &d in dependents {
                    if result.insert(d) {
                        queue.push_back(d);
                    }
                }
            }
        }
        result.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_best_analysis_order() {
        let mut map = HashMap::new();
        // 文件1依赖文件2
        map.insert(FileId::new(1), {
            let mut s = HashSet::new();
            s.insert(FileId::new(2));
            s
        });
        // 文件2没有依赖
        map.insert(FileId::new(2), HashSet::new());
        let rel = FileDependencyRelation::new(&map);
        let result =
            rel.get_best_analysis_order(&[FileId::new(1), FileId::new(2)], &HashSet::default());
        // 文件2没有依赖，应该在前；文件1依赖文件2，在后
        assert_eq!(result, vec![FileId::new(2), FileId::new(1)]);
    }

    #[test]
    fn test_best_analysis_order2() {
        let mut map = HashMap::new();
        // 文件1依赖文件2和文件3
        map.insert(1.into(), {
            let mut s = HashSet::new();
            s.insert(2.into());
            s.insert(3.into());
            s
        });
        // 文件2依赖文件3
        map.insert(2.into(), {
            let mut s = HashSet::new();
            s.insert(3.into());
            s
        });
        // 文件3没有依赖
        map.insert(3.into(), HashSet::new());
        let rel = FileDependencyRelation::new(&map);
        let result =
            rel.get_best_analysis_order(&[1.into(), 2.into(), 3.into()], &HashSet::default());
        // 文件3没有依赖，应该在最前面；然后是2，最后是1
        assert_eq!(result, vec![3.into(), 2.into(), 1.into()]);
    }

    #[test]
    fn test_no_deps_files_first() {
        let mut map = HashMap::new();
        // 文件1依赖文件2
        map.insert(FileId::new(1), {
            let mut s = HashSet::new();
            s.insert(FileId::new(2));
            s
        });
        // 文件2依赖文件1（循环依赖）
        map.insert(FileId::new(2), {
            let mut s = HashSet::new();
            s.insert(FileId::new(1));
            s
        });
        // 文件3没有依赖
        map.insert(FileId::new(3), HashSet::new());
        // 文件4没有依赖
        map.insert(FileId::new(4), HashSet::new());

        let rel = FileDependencyRelation::new(&map);
        let result = rel.get_best_analysis_order(
            &[
                FileId::new(1),
                FileId::new(2),
                FileId::new(3),
                FileId::new(4),
            ],
            &HashSet::default(),
        );

        // 文件3和4没有依赖，应该在前面
        assert_eq!(result[0], FileId::new(3));
        assert_eq!(result[1], FileId::new(4));
        // 文件1和2有循环依赖，在后面
        assert!(result.contains(&FileId::new(1)));
        assert!(result.contains(&FileId::new(2)));
    }

    #[test]
    fn the_analysis_order_places_every_dependency_before_its_dependents() {
        let mut map = HashMap::new();
        map.insert(1.into(), [2.into(), 3.into()].into_iter().collect());
        map.insert(2.into(), [3.into()].into_iter().collect());
        map.insert(3.into(), HashSet::new());
        map.insert(4.into(), [1.into()].into_iter().collect());
        map.insert(5.into(), HashSet::new());
        let rel = FileDependencyRelation::new(&map);
        let files: Vec<FileId> = (1..=5).map(FileId::new).collect();
        let metas = HashSet::from_iter([FileId::new(5)]);

        let order = rel.get_best_analysis_order(&files, &metas);
        assert_eq!(order.len(), files.len());

        let position = |file: FileId| order.iter().position(|&f| f == file).expect("file ordered");
        for (&file, deps) in &map {
            for &dep in deps {
                assert!(
                    position(dep) < position(file),
                    "{dep:?} is a dependency of {file:?} but was ordered after it: {order:?}"
                );
            }
        }

        // A meta file depends on nothing, so it must lead rather than merely
        // land somewhere legal.
        assert_eq!(order.first(), Some(&FileId::new(5)));
    }

    #[test]
    fn a_level_never_contains_a_file_depending_on_a_sibling() {
        let mut map = HashMap::new();
        map.insert(1.into(), [2.into(), 3.into()].into_iter().collect());
        map.insert(2.into(), [3.into()].into_iter().collect());
        map.insert(3.into(), HashSet::new());
        map.insert(4.into(), [1.into()].into_iter().collect());
        map.insert(5.into(), HashSet::new());
        let rel = FileDependencyRelation::new(&map);
        let files: Vec<FileId> = (1..=5).map(FileId::new).collect();

        let levels = rel.get_analysis_levels(&files, &HashSet::default());

        for level in &levels {
            for file in level {
                let deps = &map[file];
                assert!(
                    !level.iter().any(|sibling| deps.contains(sibling)),
                    "{file:?} depends on a file in its own level {level:?}"
                );
            }
        }
    }

    #[test]
    fn cyclic_files_each_get_their_own_level() {
        let mut map = HashMap::new();
        map.insert(1.into(), [2.into()].into_iter().collect());
        map.insert(2.into(), [1.into()].into_iter().collect());
        map.insert(3.into(), HashSet::new());
        let rel = FileDependencyRelation::new(&map);
        let files: Vec<FileId> = (1..=3).map(FileId::new).collect();

        let levels = rel.get_analysis_levels(&files, &HashSet::default());

        assert_eq!(levels[0], vec![FileId::new(3)]);
        assert_eq!(levels[1], vec![FileId::new(1)]);
        assert_eq!(levels[2], vec![FileId::new(2)]);
    }

    #[test]
    fn test_collect_file_dependents() {
        let mut deps = HashMap::new();
        deps.insert(
            FileId::new(1),
            [FileId::new(2), FileId::new(3)].iter().cloned().collect(),
        );
        deps.insert(FileId::new(2), [FileId::new(3)].iter().cloned().collect());
        deps.insert(FileId::new(3), HashSet::new());
        deps.insert(FileId::new(4), [FileId::new(3)].iter().cloned().collect());

        let rel = FileDependencyRelation::new(&deps);
        let mut result = rel.collect_file_dependents(vec![FileId::new(3)]);
        result.sort();
        assert_eq!(result, vec![FileId::new(1), FileId::new(2), FileId::new(4)]);
    }

    #[test]
    fn test_meta_files_first() {
        let mut map = HashMap::new();
        // 所有文件都没有依赖
        map.insert(FileId::new(1), HashSet::new());
        map.insert(FileId::new(2), HashSet::new());
        map.insert(FileId::new(3), HashSet::new());
        map.insert(FileId::new(4), HashSet::new());

        let rel = FileDependencyRelation::new(&map);

        // 文件2和4是meta文件
        let mut metas = HashSet::new();
        metas.insert(FileId::new(2));
        metas.insert(FileId::new(4));

        let result = rel.get_best_analysis_order(
            &[
                FileId::new(1),
                FileId::new(2),
                FileId::new(3),
                FileId::new(4),
            ],
            &metas,
        );

        // meta文件应该在前面（2和4），非meta文件在后面（1和3）
        assert!(metas.contains(&result[0]), "第一个应该是meta文件");
        assert!(metas.contains(&result[1]), "第二个应该是meta文件");
        assert!(!metas.contains(&result[2]), "第三个应该是非meta文件");
        assert!(!metas.contains(&result[3]), "第四个应该是非meta文件");
    }

    #[test]
    fn test_meta_with_dependencies() {
        let mut map = HashMap::new();
        // File 1 depends on file 2 (meta)
        map.insert(FileId::new(1), {
            let mut s = HashSet::new();
            s.insert(FileId::new(2));
            s
        });
        // File 2 (meta) has no dependencies
        map.insert(FileId::new(2), HashSet::new());
        // File 3 has no dependencies
        map.insert(FileId::new(3), HashSet::new());

        let rel = FileDependencyRelation::new(&map);

        let mut metas = HashSet::new();
        metas.insert(FileId::new(2));

        let result =
            rel.get_best_analysis_order(&[FileId::new(1), FileId::new(2), FileId::new(3)], &metas);

        // File 2 is meta and has no dependencies, should be first
        // File 3 has no dependencies but is not meta, should be second
        // File 1 depends on file 2, should be last
        assert_eq!(result[0], FileId::new(2), "meta file should be first");
        assert_eq!(
            result[1],
            FileId::new(3),
            "non-meta file with no dependencies should be second"
        );
        assert_eq!(
            result[2],
            FileId::new(1),
            "file with dependencies should be last"
        );
    }
}
