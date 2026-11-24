//! The inferred descriptor as `bayplan.toml`, each value followed or preceded by why it is there.

use std::fmt::Write as _;

use super::{Inference, Why};

fn quoted(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| quoted(item)).collect();
    format!("[{}]", quoted.join(", "))
}

/// A top-level list of paths, one per line with its reason, or nothing when empty.
fn paths(out: &mut String, key: &str, about: &str, values: &[Why<String>]) {
    if values.is_empty() {
        return;
    }
    let width = values
        .iter()
        .map(|v| quoted(&v.value).len())
        .max()
        .unwrap_or(0)
        + 1;
    let _ = writeln!(out, "# {about}\n{key} = [");
    for value in values {
        let item = format!("{},", quoted(&value.value));
        let _ = writeln!(out, "    {item:<width$} # {}", value.why);
    }
    out.push_str("]\n\n");
}

pub fn render(inference: &Inference) -> String {
    let mut out = String::from(
        "# Inferred by `bp init` from the Compose configuration of the main stack and git: each line says where\n\
         # it comes from. Review it, keep what is right, fix what is not; what this file states wins.\n\n",
    );
    paths(
        &mut out,
        "link",
        "Untracked files Compose reads on the host, linked into each worktree",
        &inference.link,
    );
    paths(
        &mut out,
        "copy",
        "Untracked files read inside containers, copied into each worktree",
        &inference.copy,
    );
    paths(
        &mut out,
        "rewrite_files",
        "Mounted files naming services, rewritten for each bay",
        &inference.rewrite_files,
    );

    if let Some(router) = &inference.router {
        let _ = writeln!(
            out,
            "# {}\n[router]\nkind = \"traefik\"\ndomain = {}\n",
            router.why,
            quoted(&router.value)
        );
    }

    out.push_str("# Proposed: one group per service the router exposes, then one per other service built here\n[groups]\n");
    let width = inference
        .groups
        .iter()
        .map(|group| format!("{} = {}", quoted(&group.value.0), list(&group.value.1)).len())
        .max()
        .unwrap_or(0);
    for group in &inference.groups {
        let line = format!("{} = {}", quoted(&group.value.0), list(&group.value.1));
        let _ = writeln!(out, "{line:<width$}  # {}", group.why);
    }
    out.push('\n');

    for dep in &inference.deps {
        let value = &dep.value;
        let _ = writeln!(
            out,
            "# {}\n[[deps]]\npath = {}\nlockfile = {}\ninstall = {{ services = {}, command = {} }}\n",
            dep.why,
            quoted(&value.path),
            quoted(&value.lockfile),
            list(&value.services),
            list(&value.command)
        );
    }

    resources(&mut out, inference);

    if !inference.notes.is_empty() {
        out.push_str("# Not inferred:\n");
        for note in &inference.notes {
            let _ = writeln!(out, "# - {note}");
        }
    }
    out
}

fn resources(out: &mut String, inference: &Inference) {
    if let Some(mysql) = &inference.mysql {
        let value = &mysql.value;
        let _ = writeln!(
            out,
            "# {}\n[resources.mysql]\nservice = {}\nroot_password_env = {}\ndatabase = {}\nuser = {}\ndatabase_vars = {}",
            mysql.why,
            quoted(&value.service),
            quoted(&value.root_password_env),
            quoted(&value.database),
            quoted(&value.user),
            list(&value.database_vars)
        );
        if let Some((client, dump)) = &value.tools {
            let _ = writeln!(out, "client = {}\ndump = {}", quoted(client), quoted(dump));
        }
        out.push('\n');
    }

    if let Some(rabbitmq) = &inference.rabbitmq {
        let value = &rabbitmq.value;
        let _ = writeln!(
            out,
            "# {}\n[resources.rabbitmq]\nservice = {}\nuser = {}\ndsn_vars = {}\n",
            rabbitmq.why,
            quoted(&value.service),
            quoted(&value.user),
            list(&value.dsn_vars)
        );
    }

    if let Some(redis) = &inference.redis {
        let value = &redis.value;
        let _ = writeln!(
            out,
            "# {}\n[resources.redis]\nservice = {}\nindexes = [{}, {}]\nindex_vars = {}\ndsn_vars = {}\n",
            redis.why,
            quoted(&value.service),
            value.indexes[0],
            value.indexes[1],
            list(&value.index_vars),
            list(&value.dsn_vars)
        );
    }
}
