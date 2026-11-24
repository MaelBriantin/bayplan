use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::compose::runner::{self, Project};
use crate::compose::transform::{self, Env, Input, env_of, suffixed};
use crate::config::{self, BayConfig, Descriptor, Layout, read_toml};
use crate::lock::Lock;
use crate::resources::{self, Context, Resource};
use crate::state::BayState;
use crate::{deps, git, registration, router};

struct Bay {
    name: String,
    layout: Layout,
    descriptor: Descriptor,
}

impl Bay {
    fn open(name: &str) -> Result<Self> {
        config::validate_bay_name(name)?;
        let layout = Layout::discover()?;
        let path = layout.descriptor();
        if !path.exists() {
            bail!(
                "no descriptor for this repository: create {}",
                path.display()
            );
        }
        let descriptor = read_toml(&path)?;
        Ok(Self {
            name: name.to_owned(),
            layout,
            descriptor,
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
                bail!(
                    "unknown group `{group}` in the config of bay `{}`",
                    self.name
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
    let bay = Bay::open(name)?;
    let project = bay.project();
    if !project.file.exists() {
        bail!("bay `{name}` was never started");
    }
    project.down(false)
}

pub fn rm(name: &str, force: bool) -> Result<()> {
    let bay = Bay::open(name)?;
    let _lock = Lock::acquire(&bay.layout.lock())?;
    let worktree = bay.worktree();
    if worktree.exists() && !force {
        let mut risks = Vec::new();
        if git::is_dirty(&worktree)? {
            risks.push("uncommitted changes");
        }
        if git::is_unreferenced(&worktree)? {
            risks.push("commits on no branch, tag or remote branch, which would be lost");
        }
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
    for resource in resources::from_config(&bay.descriptor.resources) {
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

pub fn ls() -> Result<()> {
    let layout = Layout::discover()?;
    let bays = layout.bays()?;
    if bays.is_empty() {
        println!("No bay: create .bayplan/bays/<bay>.toml, then run `bp up <bay>`");
        return Ok(());
    }
    println!("{:<20} {:<14} {:<30} HOSTS", "BAY", "STATUS", "HEAD");
    for bay in bays {
        let state = BayState::load(&layout.bay_dir(&bay).join("state.json"))?;
        let containers = runner::containers(&layout.bay_project(&bay)).unwrap_or_default();
        let running = containers
            .iter()
            .filter(|c| c.get("State").and_then(|s| s.as_str()) == Some("running"))
            .count();
        let status = match containers.len() {
            0 => "down".to_owned(),
            total => format!("up {running}/{total}"),
        };
        let worktree = layout.worktree(&bay);
        let head = if worktree.exists() {
            let short = git::short_head(&worktree).unwrap_or_else(|_| "?".to_owned());
            match git::current_branch(&worktree) {
                Some(branch) => format!("{branch}@{short}"),
                None => short,
            }
        } else {
            "-".to_owned()
        };
        let hosts: Vec<_> = state
            .hosts
            .values()
            .map(|h| format!("https://{h}"))
            .collect();
        println!("{bay:<20} {status:<14} {head:<30} {}", hosts.join(" "));
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
    use super::*;
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
}
