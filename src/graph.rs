use crate::manifest::{BridgeTarget, Package, PackageId};
use crate::tasks::ResolvedTask;
use crate::tasks::TaskRegistry;
use anyhow::{Result, anyhow, bail};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::io::IsTerminal;

#[derive(Debug)]
pub struct WorkspaceGraph {
    packages: BTreeMap<PackageId, Package>,
}

impl WorkspaceGraph {
    pub fn new(packages: Vec<Package>) -> Self {
        let packages = packages
            .into_iter()
            .map(|pkg| (pkg.id.clone(), pkg))
            .collect::<BTreeMap<_, _>>();
        Self { packages }
    }

    pub fn render_tree(&self) -> Result<String> {
        let _ = self.topological_order()?;
        let dependent_counts = self.dependent_counts();
        let use_color = output_uses_color();
        let roots = self
            .packages
            .keys()
            .filter(|id| dependent_counts.get(*id).copied().unwrap_or(0) == 0)
            .cloned()
            .collect::<Vec<_>>();

        let mut lines = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            let is_last_root = index + 1 == roots.len();
            self.render_package(root, "", is_last_root, true, use_color, &mut lines);
            if !is_last_root {
                lines.push(String::new());
            }
        }

        Ok(lines.join("\n"))
    }

    pub fn topological_order(&self) -> Result<Vec<&Package>> {
        let mut indegree = self
            .packages
            .keys()
            .map(|id| (id.clone(), 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut outgoing = self
            .packages
            .keys()
            .map(|id| (id.clone(), Vec::<PackageId>::new()))
            .collect::<BTreeMap<_, _>>();

        for package in self.packages.values() {
            for dep in &package.internal_dependencies {
                let entry = indegree
                    .get_mut(&package.id)
                    .expect("package must exist in indegree");
                *entry += 1;
                outgoing
                    .get_mut(dep)
                    .expect("dependency must exist in outgoing")
                    .push(package.id.clone());
            }
        }

        let mut queue = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| id.clone())
            .collect::<VecDeque<_>>();
        let mut ordered = Vec::with_capacity(self.packages.len());

        while let Some(id) = queue.pop_front() {
            let package = self.packages.get(&id).expect("queued package must exist");
            ordered.push(package);

            for next in outgoing.get(&id).expect("outgoing entry must exist").iter() {
                let degree = indegree
                    .get_mut(next)
                    .expect("dependent package must exist");
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(next.clone());
                }
            }
        }

        if ordered.len() != self.packages.len() {
            let blocked = indegree
                .into_iter()
                .filter(|(_, degree)| *degree > 0)
                .map(|(id, _)| id.to_string())
                .collect::<Vec<_>>();
            bail!(
                "cycle detected in workspace dependency graph involving: {}",
                blocked.join(", ")
            );
        }

        Ok(ordered)
    }

    pub fn render_task_plan_tree(&self, tasks: &TaskRegistry, task: &str) -> Result<String> {
        let plan = self.materialize_task_plan(tasks, task)?;
        let mut roots = plan.roots();
        let mut depths = BTreeMap::new();
        plan.sort_nodes_by_depth(&mut roots, &mut depths);
        let roots = self.materialize_display_roots(&roots, &plan, tasks)?;

        let mut lines = Vec::new();
        let use_color = output_uses_color();
        for (index, root) in roots.iter().enumerate() {
            let is_last_root = index + 1 == roots.len();
            self.render_plan_display_node(root, "", is_last_root, true, use_color, &mut lines);
            if !is_last_root {
                lines.push(String::new());
            }
        }

        Ok(lines.join("\n"))
    }

    pub fn task_plan(&self, tasks: &TaskRegistry, task: &str) -> Result<Vec<ResolvedTask>> {
        let plan = self.materialize_task_plan(tasks, task)?;
        let order = plan.priority_order()?;
        order
            .into_iter()
            .map(|node| {
                let package = self
                    .packages
                    .get(&node.package_id)
                    .expect("task node package must exist");
                tasks.resolve(package, &node.task_name)
            })
            .collect()
    }

    pub fn task_ready_groups(
        &self,
        tasks: &TaskRegistry,
        task: &str,
    ) -> Result<Vec<Vec<ResolvedTask>>> {
        let plan = self.materialize_task_plan(tasks, task)?;
        let groups = plan.ready_groups()?;
        groups
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|node| {
                        let package = self
                            .packages
                            .get(&node.package_id)
                            .expect("task node package must exist");
                        tasks.resolve(package, &node.task_name)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect()
    }

    fn materialize_task_plan(
        &self,
        tasks: &TaskRegistry,
        task: &str,
    ) -> Result<MaterializedTaskPlan> {
        let (included, restricted) = self.task_nodes_for(tasks, task)?;
        let mut prerequisites = BTreeMap::new();

        for node in &included {
            let mut prereqs = self
                .task_prerequisites(node, &restricted, tasks)?
                .into_iter()
                .filter(|prereq| included.contains(prereq))
                .collect::<Vec<_>>();
            prereqs.sort();
            prereqs.dedup();
            prerequisites.insert(node.clone(), prereqs);
        }

        Ok(MaterializedTaskPlan {
            included,
            prerequisites,
        })
    }

    fn dependent_counts(&self) -> BTreeMap<PackageId, usize> {
        let mut counts = self
            .packages
            .keys()
            .map(|id| (id.clone(), 0usize))
            .collect::<BTreeMap<_, _>>();

        for package in self.packages.values() {
            for dep in &package.internal_dependencies {
                let count = counts.get_mut(dep).expect("dependency must exist");
                *count += 1;
            }
        }

        counts
    }

    fn task_nodes_for(
        &self,
        tasks: &TaskRegistry,
        task: &str,
    ) -> Result<(BTreeSet<TaskNode>, BTreeSet<TaskNode>)> {
        let mut included = BTreeSet::new();
        let mut restricted = BTreeSet::new();
        let mut expanded = BTreeMap::new();
        let entrypoints = self
            .packages
            .values()
            .filter(|pkg| pkg.opts_into_task(task))
            .map(|pkg| pkg.id.clone())
            .collect::<Vec<_>>();

        let seed_ids = if entrypoints.is_empty() {
            self.packages
                .values()
                .filter(|pkg| tasks.seeds_all_packages_without_explicit_opt_in(pkg, task))
                .map(|pkg| pkg.id.clone())
                .collect::<Vec<_>>()
        } else {
            entrypoints
        };

        for package_id in seed_ids {
            self.collect_task_nodes(
                &package_id,
                task,
                TaskInclusion::Entrypoint,
                &mut included,
                &mut restricted,
                &mut expanded,
                tasks,
            )?;
        }
        Ok((included, restricted))
    }

    fn collect_task_nodes(
        &self,
        id: &PackageId,
        task: &str,
        reason: TaskInclusion,
        included: &mut BTreeSet<TaskNode>,
        restricted: &mut BTreeSet<TaskNode>,
        expanded: &mut BTreeMap<TaskNode, bool>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        let package = self.packages.get(id).expect("package must exist");
        if !self.should_include_task(package, task, reason, tasks)? {
            return Ok(());
        }

        let node = TaskNode::new(id.clone(), task);
        if matches!(reason, TaskInclusion::EcosystemTaskDependency) {
            restricted.insert(node.clone());
        } else {
            restricted.remove(&node);
        }
        let already_inserted = !included.insert(node.clone());
        let already_expanded = expanded.get(&node).copied().unwrap_or(false);
        let is_entrypoint_package = matches!(reason, TaskInclusion::Entrypoint);
        if already_inserted && (already_expanded || !is_entrypoint_package) {
            return Ok(());
        }
        expanded.insert(node, is_entrypoint_package || already_expanded);
        self.collect_task_dependency_nodes(id, task, included, restricted, expanded, tasks)?;
        if matches!(reason, TaskInclusion::EcosystemTaskDependency) {
            return Ok(());
        }
        if tasks.cascades_to_native_dependencies(task)? {
            self.collect_propagated_task_nodes(id, task, included, restricted, expanded, tasks)?;
        } else {
            self.collect_bridged_task_nodes(
                id,
                task,
                included,
                restricted,
                expanded,
                &mut BTreeSet::new(),
                tasks,
            )?;
        }
        Ok(())
    }

    fn collect_task_dependency_nodes(
        &self,
        id: &PackageId,
        task: &str,
        included: &mut BTreeSet<TaskNode>,
        restricted: &mut BTreeSet<TaskNode>,
        expanded: &mut BTreeMap<TaskNode, bool>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        for dependency_task in tasks.task_dependencies(task)? {
            self.collect_task_nodes(
                id,
                &dependency_task,
                TaskInclusion::TaskDependency,
                included,
                restricted,
                expanded,
                tasks,
            )?;
        }
        let source_package = self.packages.get(id).expect("package must exist");
        for dependency in tasks.ecosystem_task_dependencies(task, source_package.ecosystem)? {
            for target_id in self.reachable_packages_matching(id, dependency.ecosystem)? {
                self.collect_task_nodes(
                    &target_id,
                    &dependency.task_name,
                    TaskInclusion::EcosystemTaskDependency,
                    included,
                    restricted,
                    expanded,
                    tasks,
                )?;
            }
        }
        Ok(())
    }

    fn collect_propagated_task_nodes(
        &self,
        id: &PackageId,
        task: &str,
        included: &mut BTreeSet<TaskNode>,
        restricted: &mut BTreeSet<TaskNode>,
        expanded: &mut BTreeMap<TaskNode, bool>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        let package = self.packages.get(id).expect("package must exist");

        for dep in &package.internal_dependencies {
            self.collect_task_nodes(
                dep,
                task,
                TaskInclusion::NativeCascade,
                included,
                restricted,
                expanded,
                tasks,
            )?;
        }

        for bridge in &package.bridged_dependencies {
            self.collect_bridge_target_task(
                id, bridge, task, included, restricted, expanded, tasks,
            )?;
        }

        Ok(())
    }

    fn collect_bridged_task_nodes(
        &self,
        id: &PackageId,
        task: &str,
        included: &mut BTreeSet<TaskNode>,
        restricted: &mut BTreeSet<TaskNode>,
        expanded: &mut BTreeMap<TaskNode, bool>,
        visited_native: &mut BTreeSet<PackageId>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        if !visited_native.insert(id.clone()) {
            return Ok(());
        }

        let package = self.packages.get(id).expect("package must exist");
        for dep in &package.internal_dependencies {
            self.collect_bridged_task_nodes(
                dep,
                task,
                included,
                restricted,
                expanded,
                visited_native,
                tasks,
            )?;
        }
        for bridge in &package.bridged_dependencies {
            self.collect_bridge_target_task(
                id, bridge, task, included, restricted, expanded, tasks,
            )?;
        }
        Ok(())
    }

    fn task_prerequisites(
        &self,
        node: &TaskNode,
        restricted: &BTreeSet<TaskNode>,
        tasks: &TaskRegistry,
    ) -> Result<Vec<TaskNode>> {
        let mut prereqs = Vec::new();
        prereqs.extend(
            tasks
                .task_dependencies(&node.task_name)?
                .into_iter()
                .map(|task| TaskNode::new(node.package_id.clone(), task)),
        );
        let source_package = self
            .packages
            .get(&node.package_id)
            .expect("task node package must exist");
        for dependency in
            tasks.ecosystem_task_dependencies(&node.task_name, source_package.ecosystem)?
        {
            prereqs.extend(
                self.reachable_packages_matching(&node.package_id, dependency.ecosystem)?
                    .into_iter()
                    .map(|id| TaskNode::new(id, &dependency.task_name)),
            );
        }

        if restricted.contains(node) {
            // Ecosystem-targeted hits do not recursively fan out through same-ecosystem deps.
        } else if tasks.cascades_to_native_dependencies(&node.task_name)? {
            prereqs.extend(self.direct_task_prerequisites(node, tasks)?);
        } else if tasks.cascades_across_packages(&node.task_name)? {
            self.collect_direct_bridge_prerequisites(
                &node.package_id,
                &node.task_name,
                &mut prereqs,
                &mut BTreeSet::new(),
                tasks,
            )?;
        } else {
            // `cascade = "none"` keeps the task on explicitly opted-in packages only.
        }
        Ok(prereqs)
    }

    fn reachable_packages_matching(
        &self,
        id: &PackageId,
        ecosystem: crate::manifest::Ecosystem,
    ) -> Result<Vec<PackageId>> {
        let mut visited = BTreeSet::new();
        let mut matches = BTreeSet::new();
        self.collect_reachable_packages_matching(id, ecosystem, &mut visited, &mut matches)?;
        Ok(matches.into_iter().collect())
    }

    fn collect_reachable_packages_matching(
        &self,
        id: &PackageId,
        ecosystem: crate::manifest::Ecosystem,
        visited: &mut BTreeSet<PackageId>,
        matches: &mut BTreeSet<PackageId>,
    ) -> Result<()> {
        if !visited.insert(id.clone()) {
            return Ok(());
        }

        let package = self.packages.get(id).expect("package must exist");
        for dep in &package.internal_dependencies {
            if self
                .packages
                .get(dep)
                .is_some_and(|package| package.ecosystem == ecosystem)
            {
                matches.insert(dep.clone());
                continue;
            }
            self.collect_reachable_packages_matching(dep, ecosystem, visited, matches)?;
        }

        for bridge in &package.bridged_dependencies {
            let target_id = self.package_id_by_bridge(bridge)?;
            if self
                .packages
                .get(&target_id)
                .is_some_and(|package| package.ecosystem == ecosystem)
            {
                matches.insert(target_id.clone());
                continue;
            }
            self.collect_reachable_packages_matching(&target_id, ecosystem, visited, matches)?;
        }

        Ok(())
    }

    fn direct_task_prerequisites(
        &self,
        node: &TaskNode,
        tasks: &TaskRegistry,
    ) -> Result<Vec<TaskNode>> {
        let package = self
            .packages
            .get(&node.package_id)
            .expect("task node package must exist");
        let mut prereqs = package
            .internal_dependencies
            .iter()
            .cloned()
            .map(|id| TaskNode::new(id, &node.task_name))
            .collect::<Vec<_>>();

        for bridge in &package.bridged_dependencies {
            let target_id = self.package_id_by_bridge(bridge)?;
            let target_package = self
                .packages
                .get(&target_id)
                .expect("bridge target package must exist");
            if !self.should_propagate_same_task_across_bridge(
                package,
                target_package,
                &node.task_name,
                tasks,
            )? {
                continue;
            }
            prereqs.push(TaskNode::new(target_id, &node.task_name));
        }

        Ok(prereqs)
    }

    fn collect_direct_bridge_prerequisites(
        &self,
        id: &PackageId,
        task: &str,
        prereqs: &mut Vec<TaskNode>,
        visited_native: &mut BTreeSet<PackageId>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        if !visited_native.insert(id.clone()) {
            return Ok(());
        }

        let package = self.packages.get(id).expect("package must exist");
        for dep in &package.internal_dependencies {
            self.collect_direct_bridge_prerequisites(dep, task, prereqs, visited_native, tasks)?;
        }
        for bridge in &package.bridged_dependencies {
            let target_id = self.package_id_by_bridge(bridge)?;
            let target_package = self
                .packages
                .get(&target_id)
                .expect("bridge target package must exist");
            if !self.should_propagate_same_task_across_bridge(
                package,
                target_package,
                task,
                tasks,
            )? {
                continue;
            }
            prereqs.push(TaskNode::new(target_id, task));
        }
        Ok(())
    }

    fn package_id_by_bridge(&self, bridge: &BridgeTarget) -> Result<PackageId> {
        let matches = self
            .packages
            .iter()
            .filter(|(_, package)| {
                package.name == bridge.package_name
                    && bridge
                        .ecosystem
                        .is_none_or(|ecosystem| package.ecosystem == ecosystem)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(anyhow!(
                "package `{bridge}` referenced by task dependency was not found"
            )),
            _ => Err(anyhow!(
                "package `{bridge}` referenced by task dependency is ambiguous across ecosystems"
            )),
        }
    }

    fn collect_bridge_target_task(
        &self,
        source_id: &PackageId,
        bridge: &BridgeTarget,
        task: &str,
        included: &mut BTreeSet<TaskNode>,
        restricted: &mut BTreeSet<TaskNode>,
        expanded: &mut BTreeMap<TaskNode, bool>,
        tasks: &TaskRegistry,
    ) -> Result<()> {
        let package = self.packages.get(source_id).expect("package must exist");
        let target_id = self.package_id_by_bridge(bridge)?;
        let target = self
            .packages
            .get(&target_id)
            .expect("bridge target package must exist");
        if !self.should_propagate_same_task_across_bridge(package, target, task, tasks)? {
            return Ok(());
        }
        if (!tasks.can_inherit_without_explicit_opt_in(target, task)
            || !tasks.cascades_across_packages(task)?)
            && !target.opts_into_task(task)
        {
            return Ok(());
        }
        if !tasks.participates_in_task(target, task) {
            bail!(
                "package `{}` bridges to `{}` for task `{}`, but `{}` has not opted into `{}`",
                package.name,
                bridge,
                task,
                target.id,
                task
            );
        }
        self.collect_task_nodes(
            &target_id,
            task,
            TaskInclusion::Bridge,
            included,
            restricted,
            expanded,
            tasks,
        )?;
        Ok(())
    }

    fn should_propagate_same_task_across_bridge(
        &self,
        source: &Package,
        target: &Package,
        task: &str,
        tasks: &TaskRegistry,
    ) -> Result<bool> {
        if source.ecosystem == target.ecosystem {
            return Ok(true);
        }

        Ok(!tasks.has_targeted_ecosystem_dependency(task, source.ecosystem, target.ecosystem)?)
    }

    fn should_include_task(
        &self,
        package: &Package,
        task: &str,
        reason: TaskInclusion,
        tasks: &TaskRegistry,
    ) -> Result<bool> {
        Ok(match reason {
            TaskInclusion::Entrypoint => true,
            TaskInclusion::NativeCascade => {
                package.opts_into_task(task)
                    || (tasks.cascades_to_native_dependencies(task)?
                        && tasks.can_inherit_without_explicit_opt_in(package, task))
            }
            TaskInclusion::Bridge => {
                package.opts_into_task(task)
                    || (tasks.cascades_across_packages(task)?
                        && tasks.can_inherit_without_explicit_opt_in(package, task))
            }
            TaskInclusion::TaskDependency => {
                package.opts_into_task(task)
                    || (tasks.cascades_across_packages(task)?
                        && tasks.can_inherit_without_explicit_opt_in(package, task))
            }
            TaskInclusion::EcosystemTaskDependency => tasks.participates_in_task(package, task),
        })
    }

    fn materialize_display_roots(
        &self,
        roots: &[TaskNode],
        plan: &MaterializedTaskPlan,
        tasks: &TaskRegistry,
    ) -> Result<Vec<PlanDisplayNode>> {
        let mut display_roots = Vec::new();

        for root in roots {
            let mut node = self.materialize_task_display_node(root, plan, tasks)?;
            self.sort_plan_display_tree(&mut node);
            display_roots.push(node);
        }

        Ok(display_roots)
    }

    fn materialize_task_display_node(
        &self,
        node: &TaskNode,
        plan: &MaterializedTaskPlan,
        tasks: &TaskRegistry,
    ) -> Result<PlanDisplayNode> {
        let package = self
            .packages
            .get(&node.package_id)
            .expect("task node package must exist");
        let resolved = tasks.resolve(package, &node.task_name)?;
        let mut prerequisites = plan.prerequisites.get(node).cloned().unwrap_or_default();
        let mut depths = BTreeMap::new();
        plan.sort_nodes_by_depth(&mut prerequisites, &mut depths);

        let mut children = Vec::new();
        for prerequisite in prerequisites {
            let child =
                self.materialize_prerequisite_display_node(node, &prerequisite, plan, tasks)?;
            children.push(child);
        }

        Ok(PlanDisplayNode {
            label: resolved.render(),
            kind: PlanDisplayKind::Task,
            children,
        })
    }

    fn materialize_prerequisite_display_node(
        &self,
        source: &TaskNode,
        target: &TaskNode,
        plan: &MaterializedTaskPlan,
        tasks: &TaskRegistry,
    ) -> Result<PlanDisplayNode> {
        let mut node = self.materialize_task_display_node(target, plan, tasks)?;

        if source.package_id == target.package_id {
            return Ok(node);
        }

        let Some(path) = self.find_package_path(&source.package_id, &target.package_id)? else {
            bail!(
                "could not render task path from `{}` to prerequisite `{}`",
                source,
                target
            );
        };

        let wrap_len = match path.last() {
            Some(PackagePathStep::Native(id)) if id == &target.package_id => path.len() - 1,
            _ => path.len(),
        };
        let mut pending_bridge = None;

        for step in path[..wrap_len].iter().rev() {
            node = match step {
                PackagePathStep::Bridge(bridge) => {
                    pending_bridge = Some(bridge.clone());
                    node
                }
                PackagePathStep::Native(id) => {
                    let package = self.packages.get(id).expect("path package must exist");
                    let bridge_suffix = pending_bridge
                        .take()
                        .map(|bridge| format!(" (bridge {bridge})"))
                        .unwrap_or_default();
                    PlanDisplayNode {
                        label: format!(
                            "{} [{}]{}",
                            package.name,
                            package.display_label(),
                            bridge_suffix
                        ),
                        kind: PlanDisplayKind::Path,
                        children: vec![node],
                    }
                }
            };
        }

        if let Some(bridge) = pending_bridge {
            node.label = format!("{} (bridge {bridge})", node.label);
        }

        Ok(node)
    }

    fn find_package_path(
        &self,
        source: &PackageId,
        target: &PackageId,
    ) -> Result<Option<Vec<PackagePathStep>>> {
        let mut visited = BTreeSet::new();
        self.find_package_path_recursive(source, target, &mut visited)
    }

    fn find_package_path_recursive(
        &self,
        current: &PackageId,
        target: &PackageId,
        visited: &mut BTreeSet<PackageId>,
    ) -> Result<Option<Vec<PackagePathStep>>> {
        if current == target {
            return Ok(Some(Vec::new()));
        }
        if !visited.insert(current.clone()) {
            return Ok(None);
        }

        let package = self.packages.get(current).expect("package must exist");
        for dependency in &package.internal_dependencies {
            if let Some(mut path) = self.find_package_path_recursive(dependency, target, visited)? {
                let mut full = vec![PackagePathStep::Native(dependency.clone())];
                full.append(&mut path);
                return Ok(Some(full));
            }
        }

        for bridge in &package.bridged_dependencies {
            let target_id = self.package_id_by_bridge(bridge)?;
            if let Some(mut path) = self.find_package_path_recursive(&target_id, target, visited)? {
                let mut full = vec![PackagePathStep::Bridge(bridge.clone())];
                full.append(&mut path);
                return Ok(Some(full));
            }
        }

        Ok(None)
    }

    fn sort_plan_display_tree(&self, node: &mut PlanDisplayNode) {
        for child in &mut node.children {
            self.sort_plan_display_tree(child);
        }

        node.children.sort_by(|left, right| {
            self.plan_display_depth(right)
                .cmp(&self.plan_display_depth(left))
                .then_with(|| left.label.cmp(&right.label))
        });
    }

    fn plan_display_depth(&self, node: &PlanDisplayNode) -> usize {
        node.children
            .iter()
            .map(|child| self.plan_display_depth(child))
            .max()
            .map(|depth| depth + 1)
            .unwrap_or(0)
    }

    fn render_plan_display_node(
        &self,
        node: &PlanDisplayNode,
        prefix: &str,
        is_last: bool,
        is_root: bool,
        use_color: bool,
        lines: &mut Vec<String>,
    ) {
        let branch = if is_root {
            String::new()
        } else if is_last {
            "└── ".to_string()
        } else {
            "├── ".to_string()
        };
        lines.push(format!(
            "{prefix}{branch}{}",
            node.rendered_label(use_color)
        ));

        let child_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}│   ")
        };

        let child_count = node.children.len();
        for (position, child) in node.children.iter().enumerate() {
            self.render_plan_display_node(
                child,
                &child_prefix,
                position + 1 == child_count,
                false,
                use_color,
                lines,
            );
        }
    }

    fn render_package(
        &self,
        id: &PackageId,
        prefix: &str,
        is_last: bool,
        is_root: bool,
        use_color: bool,
        lines: &mut Vec<String>,
    ) {
        let package = self.packages.get(id).expect("package must exist");
        let branch = if is_root {
            String::new()
        } else if is_last {
            "└── ".to_string()
        } else {
            "├── ".to_string()
        };

        lines.push(format!(
            "{prefix}{branch}{} [{}] {}",
            if use_color {
                format!("\x1b[1m{}\x1b[0m", package.name)
            } else {
                package.name.clone()
            },
            package.colored_display_label(use_color),
            if use_color {
                format!("\x1b[2m{}\x1b[0m", package.manifest_path.display())
            } else {
                package.manifest_path.display().to_string()
            }
        ));

        let child_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}│   ")
        };

        for (index, dep) in package.internal_dependencies.iter().enumerate() {
            let child_is_last = index + 1 == package.internal_dependencies.len();
            self.render_package(dep, &child_prefix, child_is_last, false, use_color, lines);
        }
    }
}

fn output_uses_color() -> bool {
    #[cfg(test)]
    {
        false
    }

    #[cfg(not(test))]
    {
        std::io::stdout().is_terminal()
    }
}

#[derive(Debug)]
struct MaterializedTaskPlan {
    included: BTreeSet<TaskNode>,
    prerequisites: BTreeMap<TaskNode, Vec<TaskNode>>,
}

#[derive(Debug)]
struct SchedulerState {
    indegree: BTreeMap<TaskNode, usize>,
    outgoing: BTreeMap<TaskNode, Vec<TaskNode>>,
}

impl MaterializedTaskPlan {
    fn roots(&self) -> Vec<TaskNode> {
        let depended_on = self
            .prerequisites
            .values()
            .flat_map(|prereqs| prereqs.iter().cloned())
            .collect::<BTreeSet<_>>();

        self.included
            .iter()
            .filter(|node| !depended_on.contains(*node))
            .cloned()
            .collect()
    }

    fn topological_order(&self) -> Result<Vec<TaskNode>> {
        Ok(self.priority_schedule()?.0)
    }

    fn priority_order(&self) -> Result<Vec<TaskNode>> {
        self.topological_order()
    }

    fn ready_groups(&self) -> Result<Vec<Vec<TaskNode>>> {
        Ok(self.priority_schedule()?.1)
    }

    fn priority_schedule(&self) -> Result<(Vec<TaskNode>, Vec<Vec<TaskNode>>)> {
        let mut state = self.scheduler_state();
        let mut blocked_counts = BTreeMap::new();
        for node in &self.included {
            self.blocked_descendant_count(node, &state.outgoing, &mut blocked_counts);
        }

        let mut ready = BinaryHeap::new();
        for (node, degree) in &state.indegree {
            if *degree == 0 {
                ready.push(PriorityTaskNode::new(
                    node.clone(),
                    *blocked_counts.get(node).unwrap_or(&0),
                ));
            }
        }

        let mut ordered = Vec::with_capacity(self.included.len());
        let mut groups = Vec::new();
        while !ready.is_empty() {
            let mut current = Vec::new();
            while let Some(priority_node) = ready.pop() {
                current.push(priority_node.node);
            }
            groups.push(current.clone());

            for node in current {
                ordered.push(node.clone());
                for next in state
                    .outgoing
                    .get(&node)
                    .expect("outgoing entry must exist")
                {
                    let degree = state
                        .indegree
                        .get_mut(next)
                        .expect("dependent node must exist");
                    *degree -= 1;
                    if *degree == 0 {
                        ready.push(PriorityTaskNode::new(
                            next.clone(),
                            *blocked_counts.get(next).unwrap_or(&0),
                        ));
                    }
                }
            }
        }

        if ordered.len() != self.included.len() {
            let blocked = state
                .indegree
                .into_iter()
                .filter(|(_, degree)| *degree > 0)
                .map(|(node, _)| node.to_string())
                .collect::<Vec<_>>();
            bail!(
                "cycle detected in task dependency graph involving: {}",
                blocked.join(", ")
            );
        }

        Ok((ordered, groups))
    }

    fn scheduler_state(&self) -> SchedulerState {
        let mut indegree = self
            .included
            .iter()
            .cloned()
            .map(|node| (node, 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut outgoing = self
            .included
            .iter()
            .cloned()
            .map(|node| (node, Vec::<TaskNode>::new()))
            .collect::<BTreeMap<_, _>>();

        for (node, prereqs) in &self.prerequisites {
            for prereq in prereqs {
                let entry = indegree
                    .get_mut(node)
                    .expect("task node must exist in indegree");
                *entry += 1;
                outgoing
                    .get_mut(prereq)
                    .expect("prerequisite node must exist in outgoing")
                    .push(node.clone());
            }
        }

        for children in outgoing.values_mut() {
            children.sort();
            children.dedup();
        }

        SchedulerState { indegree, outgoing }
    }

    fn blocked_descendant_count(
        &self,
        node: &TaskNode,
        outgoing: &BTreeMap<TaskNode, Vec<TaskNode>>,
        cache: &mut BTreeMap<TaskNode, usize>,
    ) -> usize {
        if let Some(count) = cache.get(node) {
            return *count;
        }

        let mut descendants = BTreeSet::new();
        self.collect_descendants(node, outgoing, &mut descendants);
        let count = descendants.len();
        cache.insert(node.clone(), count);
        count
    }

    fn collect_descendants(
        &self,
        node: &TaskNode,
        outgoing: &BTreeMap<TaskNode, Vec<TaskNode>>,
        descendants: &mut BTreeSet<TaskNode>,
    ) {
        if let Some(children) = outgoing.get(node) {
            for child in children {
                if descendants.insert(child.clone()) {
                    self.collect_descendants(child, outgoing, descendants);
                }
            }
        }
    }

    fn sort_nodes_by_depth(&self, nodes: &mut [TaskNode], depths: &mut BTreeMap<TaskNode, usize>) {
        let mut keyed = nodes
            .iter()
            .cloned()
            .map(|node| {
                let depth = self.subtree_depth(&node, depths);
                (node, depth)
            })
            .collect::<Vec<_>>();
        keyed.sort_by(|(left_node, left_depth), (right_node, right_depth)| {
            right_depth
                .cmp(left_depth)
                .then_with(|| left_node.cmp(right_node))
        });

        for (slot, (node, _)) in nodes.iter_mut().zip(keyed.into_iter()) {
            *slot = node;
        }
    }

    fn subtree_depth(&self, node: &TaskNode, depths: &mut BTreeMap<TaskNode, usize>) -> usize {
        if let Some(depth) = depths.get(node) {
            return *depth;
        }

        let depth = self
            .prerequisites
            .get(node)
            .into_iter()
            .flatten()
            .map(|prerequisite| self.subtree_depth(prerequisite, depths))
            .max()
            .map(|child| child + 1)
            .unwrap_or(0);
        depths.insert(node.clone(), depth);
        depth
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TaskNode {
    package_id: PackageId,
    task_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorityTaskNode {
    node: TaskNode,
    blocked_count: usize,
}

#[derive(Debug, Clone)]
struct PlanDisplayNode {
    label: String,
    kind: PlanDisplayKind,
    children: Vec<PlanDisplayNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanDisplayKind {
    Task,
    Path,
}

impl PlanDisplayNode {
    fn rendered_label(&self, use_color: bool) -> String {
        if !use_color {
            return self.label.clone();
        }

        match self.kind {
            PlanDisplayKind::Task => style_task_label(&self.label, true),
            PlanDisplayKind::Path => style_path_label(&self.label),
        }
    }
}

fn style_task_label(label: &str, use_color: bool) -> String {
    if !use_color {
        return label.to_string();
    }

    let (head, variables) = match label.split_once(" {") {
        Some((head, rest)) => (head, Some(format!("{{{}", rest))),
        None => (label, None),
    };
    let Some((name_and_task, bracket_rest)) = head.rsplit_once(" [") else {
        return format!("\x1b[1m{label}\x1b[0m");
    };
    let Some((name, task)) = name_and_task.rsplit_once(':') else {
        return format!("\x1b[1m{label}\x1b[0m");
    };
    let Some((ecosystem_label, suffix)) = bracket_rest.split_once(']') else {
        return format!("\x1b[1m{label}\x1b[0m");
    };
    let ecosystem = crate::manifest::colorize_display_label(
        ecosystem_label,
        match ecosystem_label {
            "cargo" => crate::manifest::Ecosystem::Cargo,
            "uv" => crate::manifest::Ecosystem::Uv,
            _ => crate::manifest::Ecosystem::Js,
        },
        match ecosystem_label {
            "npm" => Some(crate::manifest::JsPackageManager::Npm),
            "pnpm" => Some(crate::manifest::JsPackageManager::Pnpm),
            "yarn" => Some(crate::manifest::JsPackageManager::Yarn),
            "bun" => Some(crate::manifest::JsPackageManager::Bun),
            _ => None,
        },
        true,
    );
    let suffix = if suffix.is_empty() {
        String::new()
    } else {
        format!("\x1b[2m{suffix}\x1b[0m")
    };
    let variables = variables
        .map(|vars| format!(" \x1b[2m{vars}\x1b[0m"))
        .unwrap_or_default();

    format!("\x1b[1m{name}\x1b[0m:\x1b[1;97m{task}\x1b[0m [{ecosystem}]{suffix}{variables}")
}

fn style_path_label(label: &str) -> String {
    let Some((name, rest)) = label.split_once(" [") else {
        return format!("\x1b[2m{label}\x1b[0m");
    };
    let Some((ecosystem_label, suffix)) = rest.split_once(']') else {
        return format!("\x1b[2m{label}\x1b[0m");
    };
    let ecosystem = crate::manifest::colorize_display_label_dimmed(
        ecosystem_label,
        match ecosystem_label {
            "cargo" => crate::manifest::Ecosystem::Cargo,
            "uv" => crate::manifest::Ecosystem::Uv,
            _ => crate::manifest::Ecosystem::Js,
        },
        match ecosystem_label {
            "npm" => Some(crate::manifest::JsPackageManager::Npm),
            "pnpm" => Some(crate::manifest::JsPackageManager::Pnpm),
            "yarn" => Some(crate::manifest::JsPackageManager::Yarn),
            "bun" => Some(crate::manifest::JsPackageManager::Bun),
            _ => None,
        },
        true,
    );
    let suffix = if suffix.is_empty() {
        String::new()
    } else {
        format!("\x1b[2m{suffix}\x1b[0m")
    };

    format!("\x1b[2m{name}\x1b[0m [{ecosystem}]{suffix}")
}

#[derive(Debug, Clone, Copy)]
enum TaskInclusion {
    Entrypoint,
    TaskDependency,
    EcosystemTaskDependency,
    Bridge,
    NativeCascade,
}

#[derive(Debug, Clone)]
enum PackagePathStep {
    Native(PackageId),
    Bridge(BridgeTarget),
}

impl TaskNode {
    fn new(package_id: PackageId, task_name: impl Into<String>) -> Self {
        Self {
            package_id,
            task_name: task_name.into(),
        }
    }
}

impl PriorityTaskNode {
    fn new(node: TaskNode, blocked_count: usize) -> Self {
        Self {
            node,
            blocked_count,
        }
    }
}

impl Ord for PriorityTaskNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.blocked_count
            .cmp(&other.blocked_count)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for PriorityTaskNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for TaskNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.package_id, self.task_name)
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceGraph, style_task_label};
    use crate::manifest::{BridgeTarget, Ecosystem, Package, PackageId, TaskOptIn};
    use crate::tasks::TaskRegistry;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_topological_order() {
        let a = pkg("a", vec![], vec![], vec![]);
        let b = pkg("b", vec!["a"], vec![], vec![]);
        let c = pkg("c", vec!["b"], vec![], vec![]);

        let graph = WorkspaceGraph::new(vec![c, a, b]);
        let actual = graph
            .topological_order()
            .expect("graph should sort")
            .into_iter()
            .map(|pkg| pkg.name.clone())
            .collect::<Vec<_>>();

        assert_eq!(actual, vec!["a", "b", "c"]);
    }

    #[test]
    fn detects_cycles() {
        let a = Package {
            id: PackageId::new(Ecosystem::Cargo, "a"),
            name: "a".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("a/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![PackageId::new(Ecosystem::Cargo, "b")],
        };
        let b = Package {
            id: PackageId::new(Ecosystem::Cargo, "b"),
            name: "b".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("b/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![PackageId::new(Ecosystem::Cargo, "a")],
        };

        let err = WorkspaceGraph::new(vec![a, b])
            .topological_order()
            .expect_err("cycle should fail");

        assert!(err.to_string().contains("cycle detected"));
    }

    #[test]
    fn renders_ascii_tree() {
        let shared = pkg("shared", vec![], vec![], vec![]);
        let service = pkg("service", vec!["shared"], vec![], vec![]);
        let worker = pkg("worker", vec!["shared"], vec![], vec![]);

        let tree = WorkspaceGraph::new(vec![shared, service, worker])
            .render_tree()
            .expect("tree should render");

        assert_eq!(
            tree,
            "service [cargo] service/Cargo.toml\n└── shared [cargo] shared/Cargo.toml\n\nworker [cargo] worker/Cargo.toml\n└── shared [cargo] shared/Cargo.toml"
        );
    }

    #[test]
    fn task_plan_only_includes_opted_in_entrypoints() {
        let root = temp_dir("task-plan-entrypoints");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
cargo = ["cargo", "build"]
"#,
        )
        .expect("write config");

        let shared = pkg("shared", vec![], vec![], vec![]);
        let service = pkg("service", vec!["shared"], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![shared, service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["service"]);
    }

    #[test]
    fn autoapply_all_seeds_compatible_packages_without_entrypoints() {
        let root = temp_dir("task-plan-autoapply-all");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.docker]
autoapply = "all"
default = ["docker", "build", "."]
"#,
        )
        .expect("write config");

        let shared = pkg("shared", vec![], vec![], vec![]);
        let service = pkg("service", vec!["shared"], vec![], vec![]);

        let graph = WorkspaceGraph::new(vec![shared, service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "docker").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["service", "shared"]);
    }

    #[test]
    fn task_plan_includes_cross_task_dependencies() {
        let root = temp_dir("task-plan-bridges");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
cargo = ["cargo", "build"]
"#,
        )
        .expect("write config");

        let codegen = pkg("codegen", vec![], vec!["build"], vec![]);
        let service = Package {
            id: PackageId::new(Ecosystem::Cargo, "service"),
            name: "service".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("service/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![bridge("codegen")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![codegen, service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["codegen:build", "service:build"]);
    }

    #[test]
    fn task_plan_includes_global_task_dependencies_on_same_package() {
        let root = temp_dir("task-plan-global-task-deps");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
depends_on = ["test"]
cargo = ["cargo", "build"]

[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let service = pkg("service", vec![], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["service:test", "service:build"]);
    }

    #[test]
    fn bridged_cargo_tasks_run_without_explicit_opt_in_for_any_task() {
        let root = temp_dir("task-plan-bridge-opt-in");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.generate]
autoapply = "inherit"
cargo = ["cargo", "run", "--bin", "codegen"]
"#,
        )
        .expect("write config");

        let codegen = pkg("codegen", vec![], vec![], vec![]);
        let service = Package {
            id: PackageId::new(Ecosystem::Cargo, "service"),
            name: "service".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("service/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(["generate"]),
            bridged_dependencies: vec![bridge("codegen")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![codegen, service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph
            .task_plan(&registry, "generate")
            .expect("cargo bridge target should run");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["codegen:generate", "service:generate"]);
    }

    #[test]
    fn bridged_cargo_build_runs_without_explicit_opt_in() {
        let root = temp_dir("task-plan-bridge-cargo-build");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let rust_dep = pkg("rust-dep", vec![], vec![], vec![]);
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![bridge("cargo:rust-dep")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_dep, web]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["rust-dep", "web"]);
    }

    #[test]
    fn bridged_cargo_test_runs_without_explicit_opt_in() {
        let root = temp_dir("task-plan-bridge-cargo-test");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
npm = ["npm", "run", "test"]
"#,
        )
        .expect("write config");

        let rust_dep = pkg("rust-dep", vec![], vec![], vec![]);
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["test"]),
            bridged_dependencies: vec![bridge("cargo:rust-dep")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_dep, web]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "test").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["rust-dep", "web"]);
    }

    #[test]
    fn bridged_js_tasks_run_without_explicit_opt_in() {
        let root = temp_dir("task-plan-bridge-js-generate");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.generate]
autoapply = "inherit"
cargo = ["cargo", "run", "--bin", "codegen"]
npm = ["npm", "run", "generate"]
"#,
        )
        .expect("write config");

        let web = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let service = Package {
            id: PackageId::new(Ecosystem::Cargo, "service"),
            name: "service".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("service/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(["generate"]),
            bridged_dependencies: vec![bridge("js:web")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![web, service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph
            .task_plan(&registry, "generate")
            .expect("js bridge target should run");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["web:generate", "service:generate"]);
    }

    #[test]
    fn task_plan_finds_bridges_through_native_dependencies() {
        let root = temp_dir("task-plan-transitive-bridges");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let d = Package {
            id: PackageId::new(Ecosystem::Js, "d"),
            name: "d".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("d/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let c = pkg("c", vec![], vec![], vec!["d"]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![d, c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["d", "a"]);
    }

    #[test]
    fn ecosystem_scoped_task_dependencies_search_reachable_dependency_tree() {
        let root = temp_dir("task-plan-ecosystem-scoped-deps");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
npm = ["npm", "run", "build"]

[tasks.build.ecosystem_depends_on]
js = ["cargo:gen"]

[tasks.gen]
autoapply = "inherit"
cargo = ["cargo", "run", "--bin", "gen"]
"#,
        )
        .expect("write config");

        let cargo_dep = pkg("cargo-dep", vec![], vec![], vec![]);
        let js_leaf = Package {
            id: PackageId::new(Ecosystem::Js, "leaf"),
            name: "leaf".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("leaf/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![bridge("cargo:cargo-dep")],
            internal_dependencies: vec![],
        };
        let app = Package {
            id: PackageId::new(Ecosystem::Js, "app"),
            name: "app".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("app/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![],
            internal_dependencies: vec![PackageId::new(Ecosystem::Js, "leaf")],
        };

        let graph = WorkspaceGraph::new(vec![cargo_dep, js_leaf, app]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let steps = graph
            .task_plan(&registry, "build")
            .expect("resolve plan")
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["cargo-dep:gen", "app:build"]);
    }

    #[test]
    fn task_can_propagate_to_native_dependencies() {
        let root = temp_dir("task-plan-native-propagation");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "publish").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["c", "b", "a"]);
    }

    #[test]
    fn cascade_all_still_respects_autoapply_none() {
        let root = temp_dir("task-plan-native-propagation-blocked");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "none"
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "publish").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["a"]);
    }

    #[test]
    fn task_dependencies_apply_to_propagated_packages() {
        let root = temp_dir("task-plan-global-deps-with-propagation");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
depends_on = ["test"]
cascade = "all"
cargo = ["cargo", "build"]

[tasks.test]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        let index = |needle: &str| {
            steps
                .iter()
                .position(|step| step == needle)
                .expect("step should exist")
        };
        assert!(index("c:test") < index("c:build"));
        assert!(index("b:test") < index("b:build"));
        assert!(index("a:test") < index("a:build"));
        assert!(index("c:build") < index("b:build"));
        assert!(index("b:build") < index("a:build"));
    }

    #[test]
    fn non_propagating_task_dependencies_only_apply_to_packages_that_opt_in() {
        let root = temp_dir("task-plan-non-propagating-task-deps");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
depends_on = ["test"]
cascade = "all"
cargo = ["cargo", "build"]

[tasks.test]
cascade = "none"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert!(!steps.contains(&"a:test".to_string()));
        assert!(!steps.contains(&"b:test".to_string()));
        assert!(!steps.contains(&"c:test".to_string()));

        let index = |needle: &str| {
            steps
                .iter()
                .position(|step| step == needle)
                .expect("step should exist")
        };
        assert!(index("c:build") < index("b:build"));
        assert!(index("b:build") < index("a:build"));
    }

    #[test]
    fn non_propagating_task_dependencies_run_when_package_also_opts_in() {
        let root = temp_dir("task-plan-non-propagating-task-deps-opted");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
depends_on = ["test"]
cascade = "all"
cargo = ["cargo", "build"]

[tasks.test]
cascade = "none"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["build", "test"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert!(steps.contains(&"a:test".to_string()));
        assert!(!steps.contains(&"b:test".to_string()));
        assert!(!steps.contains(&"c:test".to_string()));

        let index = |needle: &str| {
            steps
                .iter()
                .position(|step| step == needle)
                .expect("step should exist")
        };
        assert!(index("a:test") < index("a:build"));
    }

    #[test]
    fn non_propagating_tasks_do_not_inherit_across_bridges() {
        let root = temp_dir("task-plan-non-propagating-bridges");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish-docker]
cascade = "none"
cargo = ["cargo", "run", "--bin", "publish-docker"]
npm = ["npm", "run", "publish-docker"]
"#,
        )
        .expect("write config");

        let rust_dep = pkg("rust-dep", vec![], vec![], vec![]);
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "@scope/web-bridge"),
            name: "@scope/web-bridge".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![bridge("cargo:rust-dep")],
            internal_dependencies: vec![],
        };
        let app = Package {
            id: PackageId::new(Ecosystem::Cargo, "app"),
            name: "app".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("app/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(["publish-docker"]),
            bridged_dependencies: vec![bridge("js:@scope/web-bridge")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_dep, web, app]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph
            .task_plan(&registry, "publish-docker")
            .expect("non-propagating bridge should be skipped");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["app:publish-docker"]);
    }

    #[test]
    fn non_propagating_bridged_packages_do_not_traverse_their_own_bridges() {
        let root = temp_dir("task-plan-non-propagating-bridges-transitive");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish-docker]
cascade = "none"
cargo = ["cargo", "run", "--bin", "publish-docker"]
npm = ["npm", "run", "publish-docker"]
"#,
        )
        .expect("write config");

        let rust_dep = Package {
            id: PackageId::new(Ecosystem::Cargo, "rust-bridge"),
            name: "rust-bridge".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("rust-bridge/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "@scope/web-bridge"),
            name: "@scope/web-bridge".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![bridge("cargo:rust-bridge")],
            internal_dependencies: vec![],
        };
        let app = Package {
            id: PackageId::new(Ecosystem::Cargo, "app"),
            name: "app".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from("app/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(["publish-docker"]),
            bridged_dependencies: vec![bridge("js:@scope/web-bridge")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_dep, web, app]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph
            .task_plan(&registry, "publish-docker")
            .expect("transitive non-propagating bridge should be skipped");
        let steps = plan
            .into_iter()
            .map(|step| format!("{}:{}", step.package_name, step.task_name))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["app:publish-docker"]);
    }

    #[test]
    fn native_propagation_and_bridges_compose() {
        let root = temp_dir("task-plan-native-propagation-bridges");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "publish"]
npm = ["npm", "publish"]
"#,
        )
        .expect("write config");

        let d = Package {
            id: PackageId::new(Ecosystem::Js, "d"),
            name: "d".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("d/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["publish"]),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let c = pkg("c", vec![], vec![], vec!["js:d"]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![d, c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "publish").expect("resolve plan");
        let packages = plan
            .into_iter()
            .map(|step| step.package_name)
            .collect::<Vec<_>>();

        assert_eq!(packages, vec!["d", "c", "b", "a"]);
    }

    #[test]
    fn renders_native_propagation_in_task_tree() {
        let root = temp_dir("task-tree-native-propagation");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let c = pkg("c", vec![], vec![], vec![]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let tree = graph
            .render_task_plan_tree(&registry, "publish")
            .expect("render task tree");

        assert_eq!(
            tree,
            "a:publish [cargo]\n└── b:publish [cargo]\n    └── c:publish [cargo]"
        );
    }

    #[test]
    fn renders_global_task_dependencies_in_task_tree() {
        let root = temp_dir("task-tree-global-task-deps");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
depends_on = ["test"]
cargo = ["cargo", "build"]

[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let service = pkg("service", vec![], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![service]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let tree = graph
            .render_task_plan_tree(&registry, "build")
            .expect("render task tree");

        assert_eq!(tree, "service:build [cargo]\n└── service:test [cargo]");
    }

    #[test]
    fn repeats_shared_nodes_in_task_tree_for_clarity() {
        let root = temp_dir("task-tree-deduped");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let shared = pkg("shared", vec![], vec![], vec![]);
        let left = pkg("left", vec!["shared"], vec![], vec![]);
        let right = pkg("right", vec!["shared"], vec![], vec![]);
        let app = pkg("app", vec!["left", "right"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![shared, left, right, app]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let tree = graph
            .render_task_plan_tree(&registry, "publish")
            .expect("render task tree");

        assert_eq!(
            tree,
            "app:publish [cargo]\n├── left:publish [cargo]\n│   └── shared:publish [cargo]\n└── right:publish [cargo]\n    └── shared:publish [cargo]"
        );
    }

    #[test]
    fn shows_shared_nodes_on_each_branch_in_task_tree() {
        let root = temp_dir("task-tree-prefers-deeper-branch");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
autoapply = "inherit"
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let shared = pkg("shared", vec![], vec![], vec![]);
        let deep = pkg("deep", vec!["shared"], vec![], vec![]);
        let left = pkg("left", vec!["deep"], vec![], vec![]);
        let right = pkg("right", vec!["shared"], vec![], vec![]);
        let app = pkg("app", vec!["right", "left"], vec!["publish"], vec![]);

        let graph = WorkspaceGraph::new(vec![shared, deep, left, right, app]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let tree = graph
            .render_task_plan_tree(&registry, "publish")
            .expect("render task tree");

        assert_eq!(
            tree,
            "app:publish [cargo]\n├── left:publish [cargo]\n│   └── deep:publish [cargo]\n│       └── shared:publish [cargo]\n└── right:publish [cargo]\n    └── shared:publish [cargo]"
        );
    }

    #[test]
    fn scoped_bridge_resolves_duplicate_names_across_ecosystems() {
        let root = temp_dir("scoped-bridge-target");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoapply = "inherit"
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let rust_codegen = pkg_with_ecosystem(Ecosystem::Cargo, "codegen", vec![], vec![], vec![]);
        let js_codegen = Package {
            id: PackageId::new(Ecosystem::Js, "codegen"),
            name: "codegen".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("codegen/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![bridge("cargo:codegen")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_codegen, js_codegen, web]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = graph.task_plan(&registry, "build").expect("resolve plan");
        let steps = plan
            .into_iter()
            .map(|step| format!("{} [{}]", step.package_name, step.display_label))
            .collect::<Vec<_>>();

        assert_eq!(steps, vec!["codegen [cargo]", "web [npm]"]);
    }

    #[test]
    fn unscoped_bridge_is_ambiguous_across_ecosystems() {
        let root = temp_dir("unscoped-bridge-ambiguous");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let rust_codegen = pkg_with_ecosystem(Ecosystem::Cargo, "codegen", vec![], vec![], vec![]);
        let js_codegen = Package {
            id: PackageId::new(Ecosystem::Js, "codegen"),
            name: "codegen".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("codegen/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let web = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("web/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![bridge("codegen")],
            internal_dependencies: vec![],
        };

        let graph = WorkspaceGraph::new(vec![rust_codegen, js_codegen, web]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let err = graph
            .task_plan(&registry, "build")
            .expect_err("unscoped bridge should be ambiguous");

        assert!(err.to_string().contains(
            "package `codegen` referenced by task dependency is ambiguous across ecosystems"
        ));
    }

    #[test]
    fn renders_task_plan_as_tree() {
        let root = temp_dir("task-plan-tree");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let d = Package {
            id: PackageId::new(Ecosystem::Js, "d"),
            name: "d".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: PathBuf::from("d/package.json"),
            js_package_manager: Some(crate::manifest::JsPackageManager::Npm),
            task_opt_ins: make_task_opt_ins(["build"]),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        };
        let c = pkg("c", vec![], vec![], vec!["d"]);
        let b = pkg("b", vec!["c"], vec![], vec![]);
        let a = pkg("a", vec!["b"], vec!["build"], vec![]);

        let graph = WorkspaceGraph::new(vec![d, c, b, a]);
        let registry = TaskRegistry::load(&root).expect("load registry");
        let tree = graph
            .render_task_plan_tree(&registry, "build")
            .expect("render task tree");

        assert_eq!(
            tree,
            "a:build [cargo]\n└── b [cargo]\n    └── c [cargo] (bridge d)\n        └── d:build [npm]"
        );
    }

    #[test]
    fn styles_task_labels_with_bridge_suffix_without_corrupting_brackets() {
        let rendered = style_task_label("rust-bridge:gen [cargo] (bridge cargo:rust-bridge)", true);

        assert!(rendered.contains("(bridge cargo:rust-bridge)"));
        assert!(!rendered.contains("(bridge cargo:rust-bridge)]"));
    }

    fn pkg(
        name: &str,
        deps: Vec<&str>,
        task_opt_ins: Vec<&str>,
        bridged_dependencies: Vec<&str>,
    ) -> Package {
        pkg_with_ecosystem(
            Ecosystem::Cargo,
            name,
            deps,
            task_opt_ins,
            bridged_dependencies,
        )
    }

    fn pkg_with_ecosystem(
        ecosystem: Ecosystem,
        name: &str,
        deps: Vec<&str>,
        task_opt_ins: Vec<&str>,
        bridged_dependencies: Vec<&str>,
    ) -> Package {
        Package {
            id: PackageId::new(ecosystem, name),
            name: name.into(),
            ecosystem,
            manifest_path: match ecosystem {
                Ecosystem::Cargo => PathBuf::from(format!("{name}/Cargo.toml")),
                Ecosystem::Js => PathBuf::from(format!("{name}/package.json")),
                Ecosystem::Uv => PathBuf::from(format!("{name}/pyproject.toml")),
            },
            js_package_manager: None,
            task_opt_ins: make_task_opt_ins(task_opt_ins),
            bridged_dependencies: bridged_dependencies.into_iter().map(bridge).collect(),
            internal_dependencies: deps
                .into_iter()
                .map(|dep| PackageId::new(ecosystem, dep))
                .collect(),
        }
    }

    fn bridge(value: &str) -> BridgeTarget {
        BridgeTarget::parse(value).expect("bridge should parse")
    }

    fn make_task_opt_ins<I, S>(tasks: I) -> std::collections::BTreeMap<String, TaskOptIn>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        tasks
            .into_iter()
            .map(|task| (task.as_ref().to_string(), TaskOptIn::default()))
            .collect()
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should work")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
