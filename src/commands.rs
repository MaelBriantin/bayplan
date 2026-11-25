use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::compose::runner::{self, Project};
use crate::compose::transform::{self, Env, Input, env_of, suffixed};
use crate::config::{self, BayConfig, Descriptor, Layout, Stated, read_toml};
use crate::lock::Lock;
use crate::resources::{self, Context, Resource};
use crate::state::BayState;
use crate::{deps, git, registration, router};

struct Bay {
    name: String,
    layout: Layout,
    descriptor: Descriptor,
    /// What the descriptor leaves out and was inferred, and what the bays share for want of a resource.
    told: Vec<String>,
}

/// What `bayplan.toml` states, or nothing without one.
fn stated(layout: &Layout) -> Result<Stated> {
    let path = layout.descriptor();
    if path.exists() {
        read_toml(&path)
    } else {
        Ok(Stated::default())
    }
}

impl Bay {
    /// The bay with the descriptor completed by what is inferred.
    fn open(name: &str) -> Result<Self> {
        config::validate_bay_name(name)?;
        let layout = Layout::discover()?;
        let (descriptor, told) = crate::infer::complete(&layout.root, stated(&layout)?)?;
        Ok(Self {
            name: name.to_owned(),
            layout,
            descriptor,
            told,
        })
    }

    /// The bay with what the descriptor states alone, enough to stop or remove it.
    fn open_to_stop(name: &str) -> Result<Self> {
        config::validate_bay_name(name)?;
        let layout = Layout::discover()?;
        let descriptor = stated(&layout)?.alone();
        Ok(Self {
            name: name.to_owned(),
            layout,
            descriptor,
            told: Vec::new(),
        })
    }

    fn root(&self) -> &Path {
        &self.layout.root
    }

    fn dir(&self) -> PathBuf {
        self.layout.bay_dir(&self.name)
    }

    fn state_path(&self) -> PathBuf {
        self.dir().join("state.json")
    }

    fn worktree(&self) -> PathBuf {
        self.layout.worktree(&self.name)
    }

    fn project(&self) -> Project {
        Project {
            name: self.layout.bay_project(&self.name),
            file: self.dir().join("compose.json"),
            dir: self.worktree(),
        }
    }

    fn config(&self) -> Result<BayConfig> {
        let path = self.layout.bay_config(&self.name);
        if !path.exists() {
            let groups: Vec<_> = self
                .descriptor
                .groups
                .keys()
                .map(|g| format!("\"{g}\""))
                .collect();
            bail!(
                "no config for bay `{}`: create {} with e.g. `groups = [{}]`",
                self.name,
                path.display(),
                groups.join(", ")
            );
        }
        read_toml(&path)
    }

    fn services(&self, config: &BayConfig) -> Result<BTreeSet<String>> {
        let mut services = BTreeSet::new();
        for group in &config.groups {
            let Some(members) = self.descriptor.groups.get(group) else {
                let known: Vec<&str> = self.descriptor.groups.keys().map(String::as_str).collect();
                bail!(
                    "unknown group `{group}` in the config of bay `{}`; the groups are: {}",
                    self.name,
                    known.join(", ")
                );
            };
            services.extend(members.iter().cloned());
        }
        if services.is_empty() {
            bail!(
                "bay `{}` has no services: add groups to its config",
                self.name
            );
        }
        Ok(services)
    }

    /// States of the other bays.
    fn other_states(&self) -> Result<Vec<BayState>> {
        self.layout
            .bays()?
            .into_iter()
            .filter(|bay| *bay != self.name)
            .map(|bay| BayState::load(&self.layout.bay_dir(&bay).join("state.json")))
            .collect()
    }

    /// Rewrites the registration files from every bay state, recreating the services whose file changed.
    fn sync_registrations(&self, state: &BayState) -> Result<()> {
        let mut states = self.other_states()?;
        states.push(BayState {
            registrations: state.registrations.clone(),
            ..BayState::default()
        });
        for registration in &self.descriptor.registrations {
            if registration::write(self.root(), registration, &states)? {
                runner::main_recreate(self.root(), &registration.service)?;
            }
        }
        Ok(())
    }
}

/// What `up` created during this run, undone if a later step fails.
#[derive(Default)]
struct Created {
    worktree: bool,
    /// Branch created for the worktree, with no commit of its own yet.
    branch: Option<String>,
    /// The bay's Compose project, started for the first time.
    project: bool,
    /// Resources that provisioned something, with their state before it.
    resources: Vec<(usize, Value)>,
}

pub fn up(name: &str, from: Option<&str>, branch: Option<&str>, build: bool) -> Result<()> {
    let bay = Bay::open(name)?;
    for line in &bay.told {
        eprintln!("{line}");
    }
    let _lock = Lock::acquire(&bay.layout.lock())?;
    let config = bay.config()?;
    let services = bay.services(&config)?;
    let resources = resources::from_config(&bay.descriptor.resources);
    for name in config.resources.keys() {
        if !resources.iter().any(|resource| resource.name() == name) {
            bail!(
                "the config of bay `{}` sets options for `{name}`, which the descriptor does not declare in [resources]",
                bay.name
            );
        }
    }
    let mut state = BayState::load(&bay.state_path())?;
    let mut created = Created::default();

    let result = create_worktree(&bay, &config, from, branch, &mut created)
        .and_then(|()| {
            provision(
                &bay,
                &config,
                &services,
                &resources,
                &mut state,
                &mut created,
            )
        })
        .and_then(|hosts| {
            bay.project().up(build)?;
            Ok(hosts)
        });
    let hosts = match result {
        Ok(hosts) => hosts,
        Err(error) => {
            rollback(&bay, &resources, &mut state, &created);
            return Err(error);
        }
    };

    println!(
        "\nBay `{name}` is up — worktree: {}",
        bay.worktree().display()
    );
    for host in hosts {
        println!("  https://{host}");
    }
    Ok(())
}

/// Creates the worktree of the bay if missing, detached or on the branch it is given.
fn create_worktree(
    bay: &Bay,
    config: &BayConfig,
    from: Option<&str>,
    branch: Option<&str>,
    created: &mut Created,
) -> Result<()> {
    let root = bay.root();
    let worktree = bay.worktree();
    let branch = branch.or(config.branch.as_deref());
    if !worktree.exists() {
        let from = from.or(config.from.as_deref());
        match branch {
            None => git::add_worktree(root, &worktree, from.unwrap_or("HEAD"))?,
            Some(branch) if git::branch_exists(root, branch) => {
                if let Some(from) = from {
                    bail!(
                        "branch `{branch}` already exists: drop `from` ({from}) to check it out, or pick another branch"
                    );
                }
                git::add_worktree_on_branch(root, &worktree, branch).with_context(|| {
                    format!("cannot check out branch `{branch}` for bay `{}`", bay.name)
                })?;
            }
            Some(branch) => {
                git::add_worktree_new_branch(root, &worktree, branch, from.unwrap_or("HEAD"))?;
                created.branch = Some(branch.to_owned());
            }
        }
        created.worktree = true;
    } else if let Some(branch) = branch
        && git::current_branch(&worktree).as_deref() != Some(branch)
    {
        eprintln!(
            "warning: the worktree of bay `{}` already exists and is not on branch `{branch}`; it is left as it is",
            bay.name
        );
    }
    Ok(())
}

fn provision(
    bay: &Bay,
    config: &BayConfig,
    services: &BTreeSet<String>,
    resources: &[Box<dyn Resource + '_>],
    state: &mut BayState,
    created: &mut Created,
) -> Result<Vec<String>> {
    let root = bay.root();
    let worktree = bay.worktree();
    let state_path = bay.state_path();

    deps::link_untracked(root, &worktree, &bay.descriptor.link)?;
    deps::copy_untracked(root, &worktree, &bay.descriptor.copy)?;
    deps::clone(root, &worktree, &bay.descriptor.deps, state)?;
    state.save(&state_path)?;

    let project = bay.project();
    let compose_config = runner::config(&worktree, &project.name)?;
    let all = compose_config["services"]
        .as_object()
        .context("the Compose configuration has no services")?;
    let missing: Vec<_> = services
        .iter()
        .filter(|service| !all.contains_key(*service))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        bail!(
            "the groups of bay `{}` name unknown Compose services: {}",
            bay.name,
            missing.join(", ")
        );
    }
    let envs: Vec<Env> = services
        .iter()
        .map(|service| env_of(&all[service]))
        .collect();
    let main_envs: Vec<Env> = all.values().map(env_of).collect();
    let main_project = runner::main_project_name(root)?;
    provision_resources(
        bay,
        config,
        resources,
        &Shares {
            envs: &envs,
            main_envs: &main_envs,
        },
        state,
        created,
    )?;

    let router = router::from_descriptor(&bay.descriptor);
    let rewrite_files: Vec<PathBuf> = bay
        .descriptor
        .rewrite_files
        .iter()
        .map(|path| worktree.join(path))
        .collect();
    let dir = bay.dir();
    let snapshot: &BayState = state;
    let env_overrides = |env: &Env| -> Env {
        resources
            .iter()
            .flat_map(|resource| resource.env(snapshot.resource(resource.name()), env))
            .collect()
    };
    let output = transform::transform(&Input {
        config: &compose_config,
        services,
        bay: &bay.name,
        project: &project.name,
        main_project: &main_project,
        router: router.as_ref(),
        rewrite_files: &rewrite_files,
        files_dir: &dir,
        env_overrides: &env_overrides,
    })?;

    for warning in &output.warnings {
        eprintln!("warning: {warning}");
    }

    std::fs::create_dir_all(&dir)?;
    for (source, destination) in &output.files {
        let text = std::fs::read_to_string(source)
            .with_context(|| format!("cannot read {}", source.display()))?;
        std::fs::write(destination, output.rewriter.apply(&text))?;
    }
    created.project = !project.file.exists();
    std::fs::write(
        &project.file,
        serde_json::to_string_pretty(&output.compose)? + "\n",
    )?;
    state.hosts = output.hosts.clone();

    install_outdated_deps(bay, services, &project, state)?;

    for registration in &bay.descriptor.registrations {
        let key = registration.env_file.to_string_lossy().into_owned();
        let lines = registration::lines(registration, &bay.name, services, &output.envs);
        if lines.is_empty() {
            state.registrations.remove(&key);
        } else {
            state.registrations.insert(key, lines);
        }
    }
    state.save(&state_path)?;
    bay.sync_registrations(state)?;

    Ok(output.hosts.into_values().collect())
}

/// Environments the resources read to size the bay's shares.
struct Shares<'a> {
    /// Original environments of the duplicated services.
    envs: &'a [Env],
    /// Environments of every service of the main stack.
    main_envs: &'a [Env],
}

/// Provisions the bay's share of every resource, saving the state after each one.
fn provision_resources(
    bay: &Bay,
    config: &BayConfig,
    resources: &[Box<dyn Resource + '_>],
    shares: &Shares,
    state: &mut BayState,
    created: &mut Created,
) -> Result<()> {
    let root = bay.root();
    let state_path = bay.state_path();
    let other_states = bay.other_states()?;
    let no_options = toml::Table::new();
    for (index, resource) in resources.iter().enumerate() {
        let name = resource.name();
        let others: Vec<_> = other_states
            .iter()
            .map(|other| other.resource(name).clone())
            .collect();
        let context = Context {
            root,
            bay: &bay.name,
            options: config.resources.get(name).unwrap_or(&no_options),
            envs: shares.envs,
            main_envs: shares.main_envs,
            others: &others,
        };
        let before = state.resource(name).clone();
        let provisioned = resource
            .provision(&context, state.resource_mut(name))
            .with_context(|| format!("cannot provision {name}"));
        state.save(&state_path)?;
        if provisioned? {
            created.resources.push((index, before));
        }
    }
    Ok(())
}

/// Installs the dependencies whose lockfile in the worktree differs from the one they were installed from.
fn install_outdated_deps(
    bay: &Bay,
    services: &BTreeSet<String>,
    project: &Project,
    state: &mut BayState,
) -> Result<()> {
    let worktree = bay.worktree();
    for dep in deps::outdated(&worktree, &bay.descriptor.deps, state)? {
        let runner = dep.install.as_ref().and_then(|install| {
            let service = install.services.iter().find(|s| services.contains(*s))?;
            Some((service, &install.command))
        });
        match runner {
            Some((service, command)) => {
                project.run(&suffixed(service, &bay.name), command)?;
                deps::mark_installed(&worktree, dep, state)?;
            }
            None => eprintln!(
                "warning: {} differs from the main tree and no bay service installs {}",
                dep.lockfile.display(),
                dep.path.display()
            ),
        }
    }
    Ok(())
}

fn rollback(
    bay: &Bay,
    resources: &[Box<dyn Resource + '_>],
    state: &mut BayState,
    created: &Created,
) {
    if created.resources.is_empty() && !created.worktree && !created.project {
        return;
    }
    eprintln!("Rolling back what this run created…");
    let project = bay.project();
    if created.project {
        // Containers first: they hold the database, the vhost and the worktree removed next
        if let Err(error) = project.down(true) {
            eprintln!("warning: cannot stop the bay: {error:#}");
        }
        let _ = std::fs::remove_file(&project.file);
    }
    for (index, before) in created.resources.iter().rev() {
        let resource = &resources[*index];
        let state = state.resource_mut(resource.name());
        if let Err(error) = resource.rollback(bay.root(), before, state) {
            eprintln!("warning: cannot undo {}: {error:#}", resource.name());
        }
    }
    if let Err(error) = state.save(&bay.state_path()) {
        eprintln!("warning: cannot save the state: {error:#}");
    }
    if created.worktree
        && let Err(error) = git::remove_worktree(bay.root(), &bay.worktree())
    {
        eprintln!("warning: cannot remove the worktree: {error:#}");
    }
    if let Some(branch) = &created.branch
        && let Err(error) = git::delete_branch(bay.root(), branch)
    {
        eprintln!("warning: cannot delete branch `{branch}`: {error:#}");
    }
}

pub fn down(name: &str) -> Result<()> {
    let bay = Bay::open_to_stop(name)?;
    let project = bay.project();
    if !project.file.exists() {
        bail!("bay `{name}` was never started");
    }
    project.down(false)
}

pub fn rm(name: &str, force: bool) -> Result<()> {
    let bay = Bay::open_to_stop(name)?;
    let resources = resources::from_config(&bay.descriptor.resources);
    remove(&bay, &resources, force, &confirm)
}

/// What removing a worktree would lose.
pub fn removal_risks(worktree: &Path) -> Result<Vec<&'static str>> {
    let mut risks = Vec::new();
    if git::is_dirty(worktree)? {
        risks.push("uncommitted changes");
    }
    if git::is_unreferenced(worktree)? {
        risks.push("commits on no branch, tag or remote branch, which would be lost");
    }
    Ok(risks)
}

fn remove(
    bay: &Bay,
    resources: &[Box<dyn Resource + '_>],
    force: bool,
    confirm: &dyn Fn(&str) -> Result<bool>,
) -> Result<()> {
    let name = &bay.name;
    let _lock = Lock::acquire(&bay.layout.lock())?;
    let worktree = bay.worktree();
    if worktree.exists() && !force {
        let risks = removal_risks(&worktree)?;
        if !risks.is_empty()
            && !confirm(&format!(
                "The worktree {} has {}. Remove it anyway?",
                worktree.display(),
                risks.join(" and ")
            ))?
        {
            bail!("aborted");
        }
    }

    let project = bay.project();
    if project.file.exists() {
        project.down(true)?;
    }

    let mut state = BayState::load(&bay.state_path())?;
    let mut failed = Vec::new();
    for resource in resources {
        if let Err(error) = resource.teardown(bay.root(), state.resource_mut(resource.name())) {
            eprintln!("warning: cannot remove {}: {error:#}", resource.name());
            failed.push(resource.name());
        }
    }
    state.registrations.clear();
    state.save(&bay.state_path())?;
    bay.sync_registrations(&state)?;
    if !failed.is_empty() {
        // The state keeps what is left, so that the shares are neither lost nor given to another bay
        bail!(
            "cannot remove {} of bay `{name}`: its state is kept in {}; fix the cause, then run `bp rm {name}` again",
            failed.join(", "),
            bay.state_path().display()
        );
    }

    let mut branch = None;
    if worktree.exists() {
        branch = git::current_branch(&worktree);
        git::remove_worktree(bay.root(), &worktree)?;
    }
    std::fs::remove_dir_all(bay.dir()).ok();
    println!(
        "Bay `{name}` removed. Its config is kept: {}",
        bay.layout.bay_config(name).display()
    );
    if let Some(branch) = branch {
        println!("Its branch `{branch}` is kept");
    }
    Ok(())
}

/// What a bay runs, as `bp ls` and `bp watch` show it.
pub struct BayStatus {
    pub name: String,
    /// Compose project of the bay.
    pub project: String,
    /// Compose file of the bay, which commands on its services need.
    pub compose_file: PathBuf,
    pub worktree: PathBuf,
    /// `branch@sha`, or the short sha alone on a detached HEAD; `None` without a worktree.
    pub head: Option<String>,
    /// Whether the worktree has uncommitted changes.
    pub dirty: bool,
    /// Commits ahead of and behind the upstream branch, when the branch has one.
    pub sync: Option<(usize, usize)>,
    pub urls: Vec<String>,
    pub containers: Vec<Container>,
    /// Each resource the descriptor declares, with what the bay has of it.
    pub resources: Vec<(String, String)>,
    /// Groups the bay duplicates, with their services.
    pub groups: Vec<(String, Vec<String>)>,
    /// Ref and branch the bay config asks the worktree to start from and check out.
    pub from: Option<String>,
    pub branch: Option<String>,
    /// Why the bay config cannot be read, when it cannot.
    pub config_error: Option<String>,
}

pub struct Container {
    /// Name of the container, as `docker stats` gives it.
    pub name: String,
    pub service: String,
    pub state: String,
    /// Healthcheck status, empty for a service without one.
    pub health: String,
    /// Exit code of a container that stopped.
    pub exit_code: i64,
}

/// `down`, or `up <running>/<containers>`.
pub fn summary(containers: &[Container]) -> String {
    let running = containers
        .iter()
        .filter(|container| container.state == "running")
        .count();
    match containers.len() {
        0 => "down".to_owned(),
        total => format!("up {running}/{total}"),
    }
}

impl BayStatus {
    pub fn summary(&self) -> String {
        summary(&self.containers)
    }
}

/// Containers of a Compose project, none when Docker cannot tell.
pub fn containers(project: &str) -> Vec<Container> {
    runner::containers(project)
        .unwrap_or_default()
        .iter()
        .map(|container| {
            let field = |name: &str| {
                container
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Container {
                name: field("Name"),
                service: field("Service"),
                state: field("State"),
                health: field("Health"),
                exit_code: container
                    .get("ExitCode")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// `branch@sha` of a working tree, or the short sha alone on a detached HEAD.
pub fn head(tree: &Path) -> String {
    let short = git::short_head(tree).unwrap_or_else(|_| "?".to_owned());
    match git::current_branch(tree) {
        Some(branch) => format!("{branch}@{short}"),
        None => short,
    }
}

pub fn statuses(layout: &Layout) -> Result<Vec<BayStatus>> {
    // What is stated alone: inferring the rest at each read would be too slow for `watch`.
    let descriptor: Option<Stated> = stated(layout).ok();
    let declared = descriptor
        .as_ref()
        .map_or_default(|descriptor| resources::from_config(&descriptor.resources));
    layout
        .bays()?
        .into_iter()
        .map(|bay| {
            let state = BayState::load(&layout.bay_dir(&bay).join("state.json"))?;
            let config_path = layout.bay_config(&bay);
            let (config, config_error) = if config_path.exists() {
                match read_toml::<BayConfig>(&config_path) {
                    Ok(config) => (Some(config), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                }
            } else {
                (None, None)
            };
            let project = layout.bay_project(&bay);
            let containers = containers(&project);
            let worktree = layout.worktree(&bay);
            let exists = worktree.exists();
            let head = exists.then(|| head(&worktree));
            let urls = state
                .hosts
                .values()
                .map(|host| format!("https://{host}"))
                .collect();
            let resources = declared
                .iter()
                .map(|resource| {
                    let name = resource.name();
                    (name.to_owned(), resource.describe(state.resource(name)))
                })
                .collect();
            let groups = config.as_ref().map_or_default(|config| {
                config
                    .groups
                    .iter()
                    .map(|group| {
                        let services = descriptor
                            .as_ref()
                            .and_then(|descriptor| descriptor.groups.as_ref()?.get(group))
                            .cloned()
                            .unwrap_or_default();
                        (group.clone(), services)
                    })
                    .collect()
            });
            Ok(BayStatus {
                compose_file: layout.bay_dir(&bay).join("compose.json"),
                name: bay,
                project,
                dirty: exists && git::is_dirty(&worktree).unwrap_or(false),
                sync: exists.then(|| git::ahead_behind(&worktree)).flatten(),
                worktree,
                head,
                urls,
                containers,
                resources,
                groups,
                from: config.as_ref().and_then(|config| config.from.clone()),
                branch: config.and_then(|config| config.branch),
                config_error,
            })
        })
        .collect()
}

pub fn ls() -> Result<()> {
    let statuses = statuses(&Layout::discover()?)?;
    if statuses.is_empty() {
        println!("No bay: create .bayplan/bays/<bay>.toml, then run `bp up <bay>`");
        return Ok(());
    }
    println!("{:<20} {:<14} {:<30} HOSTS", "BAY", "STATUS", "HEAD");
    for status in statuses {
        println!(
            "{:<20} {:<14} {:<30} {}",
            status.name,
            status.summary(),
            status.head.as_deref().unwrap_or("-"),
            status.urls.join(" ")
        );
    }
    Ok(())
}

pub fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use serde_json::json;

    use super::*;
    use crate::compose::transform::Env;
    use crate::exec::tests::FakeDocker;
    use crate::git::tests::{create_branch, repo};

    fn bay(root: &Path) -> Bay {
        Bay {
            name: "demo".to_owned(),
            layout: Layout::new(root.to_path_buf()),
            descriptor: toml::from_str(
                r#"
                [router]
                kind = "traefik"
                domain = "app.localhost"
                [groups]
                "#,
            )
            .unwrap(),
            told: Vec::new(),
        }
    }

    fn config(toml: &str) -> BayConfig {
        toml::from_str(&format!("groups = []\n{toml}")).unwrap()
    }

    #[test]
    fn creates_a_detached_worktree_without_a_branch() {
        let root = repo("bay-detached");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, None, &mut created).unwrap();
        assert!(created.worktree);
        assert_eq!(created.branch, None);
        assert_eq!(git::current_branch(&bay.worktree()), None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn creates_the_branch_when_missing() {
        let root = repo("bay-new-branch");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, Some("feat/x"), &mut created).unwrap();
        assert_eq!(created.branch.as_deref(), Some("feat/x"));
        assert_eq!(
            git::current_branch(&bay.worktree()).as_deref(),
            Some("feat/x")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn takes_the_branch_from_the_bay_config() {
        let root = repo("bay-config-branch");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(
            &bay,
            &config(r#"branch = "feat/x""#),
            None,
            None,
            &mut created,
        )
        .unwrap();
        assert_eq!(
            git::current_branch(&bay.worktree()).as_deref(),
            Some("feat/x")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn checks_out_an_existing_branch_without_owning_it() {
        let root = repo("bay-existing-branch");
        create_branch(&root, "feat/x");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, Some("feat/x"), &mut created).unwrap();
        assert!(created.worktree);
        assert_eq!(created.branch, None);
        assert_eq!(
            git::current_branch(&bay.worktree()).as_deref(),
            Some("feat/x")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refuses_a_start_ref_for_an_existing_branch() {
        let root = repo("bay-existing-branch-from");
        create_branch(&root, "feat/x");
        let bay = bay(&root);
        let mut created = Created::default();
        let result = create_worktree(
            &bay,
            &config(""),
            Some("main"),
            Some("feat/x"),
            &mut created,
        );
        assert!(result.is_err());
        assert!(!created.worktree);
        assert!(!bay.worktree().exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn leaves_an_existing_worktree_on_its_branch() {
        let root = repo("bay-existing-worktree");
        let bay = bay(&root);
        create_worktree(
            &bay,
            &config(""),
            None,
            Some("feat/x"),
            &mut Created::default(),
        )
        .unwrap();
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, Some("feat/y"), &mut created).unwrap();
        assert!(!created.worktree);
        assert_eq!(
            git::current_branch(&bay.worktree()).as_deref(),
            Some("feat/x")
        );
        assert!(!git::branch_exists(&root, "feat/y"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_removes_the_worktree_and_the_branch_it_created() {
        let root = repo("bay-rollback");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, Some("feat/x"), &mut created).unwrap();
        rollback(&bay, &[], &mut BayState::default(), &created);
        assert!(!bay.worktree().exists());
        assert!(!git::branch_exists(&root, "feat/x"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_keeps_a_branch_it_did_not_create() {
        let root = repo("bay-rollback-existing");
        create_branch(&root, "feat/x");
        let bay = bay(&root);
        let mut created = Created::default();
        create_worktree(&bay, &config(""), None, Some("feat/x"), &mut created).unwrap();
        rollback(&bay, &[], &mut BayState::default(), &created);
        assert!(!bay.worktree().exists());
        assert!(git::branch_exists(&root, "feat/x"));
        std::fs::remove_dir_all(root).ok();
    }

    /// A resource logging its teardowns, failing them when told to.
    struct Recorder {
        name: &'static str,
        fails: bool,
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Resource for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }

        fn provision(&self, _: &Context, _: &mut Value) -> Result<bool> {
            unreachable!()
        }

        fn teardown(&self, _: &Path, state: &mut Value) -> Result<()> {
            self.log.borrow_mut().push(self.name);
            if self.fails {
                bail!("{} cannot be removed", self.name);
            }
            *state = Value::Null;
            Ok(())
        }

        fn env(&self, _: &Value, _: &Env) -> Env {
            Env::new()
        }

        fn describe(&self, _: &Value) -> String {
            String::new()
        }
    }

    fn recorders(failing: &[&str], log: &Rc<RefCell<Vec<&'static str>>>) -> Vec<Box<dyn Resource>> {
        ["mysql", "rabbitmq", "redis"]
            .into_iter()
            .map(|name| -> Box<dyn Resource> {
                Box::new(Recorder {
                    name,
                    fails: failing.contains(&name),
                    log: Rc::clone(log),
                })
            })
            .collect()
    }

    /// A state holding a share of each recorder.
    fn shared_state() -> BayState {
        let mut state = BayState::default();
        for name in ["mysql", "rabbitmq", "redis"] {
            *state.resource_mut(name) = json!({"share": name});
        }
        state
    }

    fn never_asked(question: &str) -> Result<bool> {
        panic!("asked: {question}")
    }

    #[test]
    fn rollback_undoes_the_shares_in_reverse_and_goes_on_after_a_failure() {
        let root = repo("rollback-shares");
        let bay = bay(&root);
        let log = Rc::default();
        let resources = recorders(&["rabbitmq"], &log);
        let created = Created {
            resources: vec![(0, Value::Null), (1, Value::Null), (2, Value::Null)],
            ..Created::default()
        };
        let mut state = shared_state();
        rollback(&bay, &resources, &mut state, &created);
        assert_eq!(*log.borrow(), ["redis", "rabbitmq", "mysql"]);
        let saved = BayState::load(&bay.state_path()).unwrap();
        assert_eq!(saved.resources.keys().collect::<Vec<_>>(), ["rabbitmq"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_keeps_a_share_the_bay_had_before_the_run() {
        let root = repo("rollback-kept");
        let bay = bay(&root);
        let log = Rc::default();
        let resources = recorders(&[], &log);
        let created = Created {
            resources: vec![(0, json!({"share": "mysql"}))],
            ..Created::default()
        };
        rollback(&bay, &resources, &mut shared_state(), &created);
        assert!(log.borrow().is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_stops_the_project_it_started_then_removes_the_worktree() {
        let root = repo("rollback-project");
        let docker = FakeDocker::install(&root.join("docker"));
        let bay = bay(&root);
        let project = bay.project();
        git::add_worktree(&root, &bay.worktree(), "HEAD").unwrap();
        std::fs::create_dir_all(bay.dir()).unwrap();
        std::fs::write(&project.file, "{}").unwrap();
        let created = Created {
            worktree: true,
            project: true,
            ..Created::default()
        };
        rollback(&bay, &[], &mut BayState::default(), &created);
        assert_eq!(docker.calls().len(), 1);
        assert!(docker.calls()[0].ends_with("down --remove-orphans --volumes"));
        assert!(!project.file.exists());
        assert!(!bay.worktree().exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_leaves_alone_what_the_run_did_not_create() {
        let root = repo("rollback-nothing");
        let docker = FakeDocker::install(&root.join("docker"));
        let bay = bay(&root);
        git::add_worktree(&root, &bay.worktree(), "HEAD").unwrap();
        std::fs::create_dir_all(bay.dir()).unwrap();
        std::fs::write(bay.project().file, "{}").unwrap();
        rollback(&bay, &[], &mut BayState::default(), &Created::default());
        assert!(bay.worktree().exists());
        assert!(bay.project().file.exists());
        assert_eq!(docker.calls(), Vec::<String>::new());
        std::fs::remove_dir_all(root).ok();
    }

    /// A bay as `bp up` leaves it: a worktree, a started project and a share of each recorder.
    fn bay_up(name: &str) -> (PathBuf, Bay, FakeDocker) {
        let root = repo(name);
        let docker = FakeDocker::install(&root.join("docker"));
        let bay = bay(&root);
        git::add_worktree(&root, &bay.worktree(), "HEAD").unwrap();
        std::fs::create_dir_all(bay.dir()).unwrap();
        std::fs::write(bay.project().file, "{}").unwrap();
        shared_state().save(&bay.state_path()).unwrap();
        (root, bay, docker)
    }

    #[test]
    fn removes_a_clean_bay_without_asking() {
        let (root, bay, docker) = bay_up("rm-clean");
        let log = Rc::default();
        remove(&bay, &recorders(&[], &log), false, &never_asked).unwrap();
        assert!(docker.calls()[0].ends_with("down --remove-orphans --volumes"));
        assert_eq!(*log.borrow(), ["mysql", "rabbitmq", "redis"]);
        assert!(!bay.worktree().exists());
        assert!(!bay.dir().exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn keeps_everything_when_removing_uncommitted_work_is_refused() {
        let (root, bay, docker) = bay_up("rm-refused");
        std::fs::write(bay.worktree().join("draft.txt"), "wip").unwrap();
        let log = Rc::default();
        let questions = RefCell::new(Vec::new());
        let refuse = |question: &str| {
            questions.borrow_mut().push(question.to_owned());
            Ok(false)
        };
        assert!(remove(&bay, &recorders(&[], &log), false, &refuse).is_err());
        assert!(questions.borrow()[0].contains("uncommitted changes"));
        assert_eq!(docker.calls(), Vec::<String>::new());
        assert!(log.borrow().is_empty());
        assert!(bay.worktree().join("draft.txt").exists());
        assert_eq!(
            BayState::load(&bay.state_path()).unwrap().resources.len(),
            3
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn asks_before_losing_commits_no_branch_holds() {
        let (root, bay, _docker) = bay_up("rm-unreferenced");
        let status = std::process::Command::new("git")
            .current_dir(bay.worktree())
            .args([
                "-c",
                "user.name=bayplan",
                "-c",
                "user.email=bayplan@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "work",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let questions = RefCell::new(Vec::new());
        let refuse = |question: &str| {
            questions.borrow_mut().push(question.to_owned());
            Ok(false)
        };
        assert!(remove(&bay, &[], false, &refuse).is_err());
        assert!(questions.borrow()[0].contains("commits on no branch"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn force_removes_uncommitted_work_without_asking() {
        let (root, bay, _docker) = bay_up("rm-force");
        std::fs::write(bay.worktree().join("draft.txt"), "wip").unwrap();
        remove(&bay, &recorders(&[], &Rc::default()), true, &never_asked).unwrap();
        assert!(!bay.worktree().exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn keeps_the_worktree_and_the_state_of_a_share_it_cannot_remove() {
        let (root, bay, _docker) = bay_up("rm-share-failed");
        let log = Rc::default();
        let error = remove(&bay, &recorders(&["rabbitmq"], &log), false, &never_asked)
            .err()
            .unwrap();
        assert!(error.to_string().contains("rabbitmq"));
        assert_eq!(*log.borrow(), ["mysql", "rabbitmq", "redis"]);
        assert!(bay.worktree().exists());
        let saved = BayState::load(&bay.state_path()).unwrap();
        assert_eq!(saved.resources.keys().collect::<Vec<_>>(), ["rabbitmq"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reads_the_status_of_each_bay() {
        let root = repo("statuses");
        let docker = FakeDocker::install(&root.join("docker"));
        docker.output(concat!(
            r#"{"Service":"web","State":"running","Health":"healthy"}"#,
            "\n",
            r#"{"Service":"api","State":"exited","Health":""}"#,
            "\n",
        ));
        let bay = bay(&root);
        create_worktree(
            &bay,
            &config(""),
            None,
            Some("feat/x"),
            &mut Created::default(),
        )
        .unwrap();
        let mut state = BayState::default();
        state.hosts.insert(
            "app.localhost".to_owned(),
            "bay-demo.app.localhost".to_owned(),
        );
        state.save(&bay.state_path()).unwrap();

        let statuses = statuses(&bay.layout).unwrap();
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.name, "demo");
        assert_eq!(status.summary(), "up 1/2");
        assert!(status.head.as_deref().unwrap().starts_with("feat/x@"));
        assert_eq!(status.urls, ["https://bay-demo.app.localhost"]);
        assert_eq!(status.containers[0].service, "web");
        assert_eq!(status.containers[0].health, "healthy");
        assert!(docker.calls()[0].ends_with(&format!("-p {} ps -a --format json", status.project)));
        std::fs::remove_dir_all(root).ok();
    }
}
