//! Built-in modules: files.
#![allow(unused_imports)]
use super::*;

pub(super) fn mode_s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn attrs(conn: &Conn, path: &str) -> Option<(String, String, String)> {
    let o = conn
        .exec(&format!(
            "stat -L -c '%a %U %G' {p} 2>/dev/null || stat -L -f '%Lp %Su %Sg' {p}",
            p = q(path)
        ))
        .ok()?;
    if o.rc != 0 {
        return None;
    }
    let mut it = o.stdout.split_whitespace();
    Some((
        it.next()?.to_string(),
        it.next()?.to_string(),
        it.next()?.to_string(),
    ))
}

/// Apply mode/owner/group; returns whether something changed.
pub(crate) fn apply_attrs(a: &Args, ctx: &Ctx, path: &str) -> Result<bool> {
    let mode = a.get("mode").filter(|v| !v.is_null()).map(mode_s);
    let owner = st(a, "owner");
    let grp = st(a, "group");
    if mode.is_none() && owner.is_none() && grp.is_none() {
        return Ok(false);
    }
    if bo(a, "recurse", false) {
        // compare a before/after snapshot of the whole tree to report `changed` accurately
        let snap = || -> String {
            let p = q(path);
            ctx.conn
                .exec(&format!(
                    "find {p} -exec stat -c '%n %a %U %G' {{}} + 2>/dev/null || find {p} -exec stat -f '%N %Lp %Su %Sg' {{}} +"
                ))
                .map(|o| o.stdout)
                .unwrap_or_default()
        };
        let before = snap();
        if ctx.check {
            return Ok(false);
        }
        for (cmd, v) in [
            ("chmod", mode.as_deref()),
            ("chown", owner.as_deref()),
            ("chgrp", grp.as_deref()),
        ] {
            if let Some(v) = v {
                let o = ctx.conn.exec(&format!("{cmd} -R {} {}", q(v), q(path)))?;
                if o.rc != 0 {
                    bail!("{cmd} failed: {}", o.stderr.trim());
                }
            }
        }
        return Ok(snap() != before);
    }
    let cur = attrs(ctx.conn, path);
    let mut changed = false;
    let rec = "";
    if let Some(m) = mode {
        let numeric = m.chars().all(|c| c.is_ascii_digit());
        let differs = match (&cur, numeric) {
            (Some((cm, _, _)), true) => cm.trim_start_matches('0') != m.trim_start_matches('0'),
            _ => false,
        };
        if differs || (cur.is_none() && !ctx.check) {
            changed = true;
            if !ctx.check {
                let o = ctx
                    .conn
                    .exec(&format!("chmod {rec}{} {}", q(&m), q(path)))?;
                if o.rc != 0 {
                    bail!("chmod failed: {}", o.stderr.trim());
                }
            }
        } else if !numeric && !ctx.check {
            ctx.conn
                .exec(&format!("chmod {rec}{} {}", q(&m), q(path)))?;
        }
    }
    for (kind, want, idx) in [("owner", owner, 1), ("group", grp, 2)] {
        if let Some(w) = want {
            let have = cur
                .as_ref()
                .map(|c| if idx == 1 { c.1.clone() } else { c.2.clone() });
            if have.as_deref() != Some(w.as_str()) {
                changed = true;
                if !ctx.check {
                    let cmd = if kind == "owner" {
                        format!("chown {rec}{} {}", q(&w), q(path))
                    } else {
                        format!("chgrp {rec}{} {}", q(&w), q(path))
                    };
                    let o = ctx.conn.exec(&cmd)?;
                    if o.rc != 0 {
                        bail!("{cmd} failed: {}", o.stderr.trim());
                    }
                }
            }
        }
    }
    Ok(changed)
}

pub(crate) fn dir_of(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => ".".into(),
    }
}

/// Ansible-style `diff` entry (rendered by the console callback with --diff).
pub(crate) fn diff_entry(old: Option<&str>, new: &str, path: &str, with_label: bool) -> Value {
    let suffix = if with_label { " (content)" } else { "" };
    serde_json::json!({
        "before": old.unwrap_or(""),
        "after": new,
        "before_header": if old.is_some() { format!("before: {path}{suffix}") } else { format!("before{}", if with_label { suffix.to_string() } else { String::new() }) },
        "after_header": format!("after: {path}{suffix}"),
    })
}

/// Write content to dest if different. Returns result.
/// Run the `validate` command (with %s = temp file) before installing content.
pub(super) fn validate_content(
    a: &Args,
    ctx: &Ctx,
    data: &[u8],
    legacy: bool,
) -> Result<Option<Res>> {
    let Some(v) = st(a, "validate") else {
        return Ok(None);
    };
    if !v.contains("%s") {
        return Ok(Some(fail("validate must contain %s")));
    }
    let tmp = ctx
        .conn
        .exec("mktemp /tmp/ransible-validate.XXXXXX")?
        .stdout
        .trim()
        .to_string();
    ctx.conn.put(&tmp, data, None)?;
    let o = ctx.conn.exec(&v.replace("%s", &q(&tmp)))?;
    let _ = ctx.conn.exec(&format!("rm -f {}", q(&tmp)));
    if o.rc != 0 {
        if legacy {
            // lineinfile/blockinfile/replace: Ansible's `fail_json(msg="failed to validate: rc:%s error:%s")`
            return Ok(Some(fail(format!(
                "failed to validate: rc:{} error:{}",
                o.rc,
                o.stderr.trim()
            ))));
        }
        let mut r = fail("failed to validate");
        r.insert("exit_status".into(), o.rc.into());
        r.insert("stdout".into(), o.stdout.clone().into());
        r.insert("stderr".into(), o.stderr.clone().into());
        for (k, t) in [("stdout_lines", &o.stdout), ("stderr_lines", &o.stderr)] {
            r.insert(
                k.into(),
                t.lines().map(Value::from).collect::<Vec<_>>().into(),
            );
        }
        return Ok(Some(r));
    }
    Ok(None)
}

/// Add the file attributes Ansible returns from file-writing modules (mode, owner, group, uid, gid, size, state).
pub(crate) fn add_file_facts(ctx: &Ctx, r: &mut Res, path: &str) {
    let p = q(path);
    let Ok(o) = ctx.conn.exec(&format!(
        "stat -c '%a|%U|%G|%u|%g|%s|%F' {p} 2>/dev/null || stat -f '%Lp|%Su|%Sg|%u|%g|%z|%HT' {p}"
    )) else {
        return;
    };
    if o.rc != 0 {
        return;
    }
    let f: Vec<&str> = o.stdout.trim().split('|').collect();
    if f.len() < 7 {
        return;
    }
    r.insert("mode".into(), format!("{:0>4}", f[0]).into());
    r.insert("owner".into(), f[1].into());
    r.insert("group".into(), f[2].into());
    r.insert(
        "uid".into(),
        f[3].parse::<i64>().map(Value::from).unwrap_or(Value::Null),
    );
    r.insert(
        "gid".into(),
        f[4].parse::<i64>().map(Value::from).unwrap_or(Value::Null),
    );
    r.insert(
        "size".into(),
        f[5].parse::<i64>().map(Value::from).unwrap_or(Value::Null),
    );
    let state = match f[6].to_lowercase().as_str() {
        "directory" => "directory",
        "symbolic link" => "link",
        _ => "file",
    };
    r.insert("state".into(), state.into());
}

/// `backup: yes` — copy `dest` to `dest.<pid>.<date>~` like Ansible; returns the backup path.
pub(crate) fn make_backup(a: &Args, ctx: &Ctx, dest: &str) -> Result<Option<String>> {
    if !bo(a, "backup", false) || ctx.check {
        return Ok(None);
    }
    let p = q(dest);
    let o = ctx.conn.exec(&format!(
        "test -e {p} || exit 0; b={p}.$$.$(date +%Y-%m-%d@%H:%M:%S)~; cp -p {p} \"$b\" && echo \"$b\""
    ))?;
    let b = o.stdout.trim().to_string();
    Ok(if b.is_empty() { None } else { Some(b) })
}

pub(crate) fn put_content(a: &Args, ctx: &Ctx, dest: &str, data: &[u8]) -> Result<Res> {
    let cur = ctx.conn.get_bytes(dest)?;
    let mut changed = cur.as_deref() != Some(data);
    if !bo(a, "force", true) && cur.is_some() {
        changed = false;
    }
    let mut r = res(false);
    let mut backup: Option<String> = None;
    if changed {
        if ctx.diff {
            let old = cur
                .as_ref()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .unwrap_or_default();
            r.insert(
                "diff".into(),
                diff_entry(
                    cur.as_ref().map(|_| old.as_str()),
                    &String::from_utf8_lossy(data),
                    dest,
                    false,
                ),
            );
        }
        if !ctx.check {
            backup = make_backup(a, ctx, dest)?;
            let o = ctx.conn.exec(&format!("mkdir -p {}", q(&dir_of(dest))))?;
            if o.rc != 0 {
                bail!(
                    "could not create destination directory: {}",
                    o.stderr.trim()
                );
            }
            if let Some(mut f) = validate_content(a, ctx, data, false)? {
                use sha1::Digest;
                f.insert(
                    "checksum".into(),
                    hex::encode(sha1::Sha1::digest(data)).into(),
                );
                return Ok(f);
            }
            ctx.conn.put(dest, data, None)?;
        }
    }
    let ac = apply_attrs(a, ctx, dest)?;
    r.insert("changed".into(), (changed || ac).into());
    if let Some(b) = backup {
        r.insert("backup_file".into(), b.into());
    }
    r.insert("dest".into(), dest.into());
    {
        use sha1::Digest;
        r.insert(
            "checksum".into(),
            hex::encode(sha1::Sha1::digest(data)).into(),
        );
    }
    add_file_facts(ctx, &mut r, dest);
    Ok(r)
}

pub(super) fn copy(a: &Args, ctx: &Ctx, templ: bool) -> Result<Res> {
    let dest =
        st_any(a, &["dest", "path"]).ok_or_else(|| anyhow!("missing required argument: dest"))?;
    let data: Vec<u8>;
    let mut dest = dest;
    let basename: String;
    if let Some(c) = a.get("content") {
        data = match c {
            Value::String(s) => s.clone().into_bytes(),
            other => serde_json::to_string(other)?.into_bytes(),
        };
        basename = String::new();
    } else {
        let src = st(a, "src").ok_or_else(|| anyhow!("missing required argument: src"))?;
        if !templ {
            let remote = bo(a, "remote_src", false);
            let is_dir = if remote {
                ctx.conn.exec(&format!("test -d {}", q(&src)))?.rc == 0
            } else {
                find_src(ctx, "files", &src)
                    .map(|p| p.is_dir())
                    .unwrap_or(false)
            };
            if is_dir {
                return copy_dir(a, ctx, &src, &dest, remote);
            }
        }
        basename = src.rsplit('/').next().unwrap_or("").to_string();
        if templ {
            let p = find_src(ctx, "templates", &src)?;
            let text = String::from_utf8(crate::vault::read_file(&p)?)
                .map_err(|e| anyhow!("{}: {e}", p.display()))?;
            let mut vars = ctx.vars.clone();
            vars.entry("ansible_managed")
                .or_insert("Ansible managed".into());
            vars.entry("template_path")
                .or_insert(p.display().to_string().into());
            data = ctx
                .templar
                .render_template_file(&text, &vars, a)?
                .into_bytes();
        } else if bo(a, "remote_src", false) {
            data = ctx
                .conn
                .get_bytes(&src)?
                .ok_or_else(|| anyhow!("remote src {src} not found"))?;
        } else {
            let p = find_src(ctx, "files", &src)?;
            if p.is_dir() {
                bail!("copy of directories is not supported yet: {}", p.display());
            }
            data = crate::vault::read_file(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
        }
    }
    if dest.ends_with('/') || ctx.conn.exec(&format!("test -d {}", q(&dest)))?.rc == 0 {
        let base = if templ {
            basename.trim_end_matches(".j2").to_string()
        } else {
            basename
        };
        dest = format!("{}/{}", dest.trim_end_matches('/'), base);
    }
    put_content(a, ctx, &dest, &data)
}

/// Recursive `copy` of a directory (local or `remote_src`), Ansible semantics: a trailing
/// slash on `src` copies the contents, otherwise the directory itself lands under `dest`.
fn copy_dir(a: &Args, ctx: &Ctx, src: &str, dest: &str, remote: bool) -> Result<Res> {
    let top = if src.ends_with('/') {
        dest.trim_end_matches('/').to_string()
    } else {
        let base = src.rsplit('/').next().unwrap_or("");
        format!("{}/{}", dest.trim_end_matches('/'), base)
    };
    // (relative path, is_dir)
    let mut entries: Vec<(String, bool)> = vec![];
    let local_root = if remote {
        None
    } else {
        Some(find_src(ctx, "files", src)?)
    };
    if let Some(root) = &local_root {
        fn walk(dir: &std::path::Path, rel: &str, out: &mut Vec<(String, bool)>) -> Result<()> {
            let mut names: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
            names.sort_by_key(|e| e.file_name());
            for e in names {
                let r = format!("{rel}{}", e.file_name().to_string_lossy());
                if e.path().is_dir() {
                    out.push((r.clone(), true));
                    walk(&e.path(), &format!("{r}/"), out)?;
                } else {
                    out.push((r, false));
                }
            }
            Ok(())
        }
        walk(root, "", &mut entries)?;
    } else {
        let o = ctx.conn.exec(&format!(
            "cd {} && find . -mindepth 1 \\( -type d -o -type f \\) | sort",
            q(src)
        ))?;
        for l in o.stdout.lines() {
            let rel = l.trim_start_matches("./").to_string();
            let d = ctx
                .conn
                .exec(&format!("test -d {}/{}", q(src), q(&rel)))?
                .rc
                == 0;
            entries.push((rel, d));
        }
    }
    let mut changed = false;
    let dmode = st(a, "directory_mode").or_else(|| st(a, "mode"));
    let mk = |d: &str| -> Result<bool> {
        let exists = ctx.conn.exec(&format!("test -d {}", q(d)))?.rc == 0;
        if !exists && !ctx.check {
            let o = ctx.conn.exec(&format!("mkdir -p {}", q(d)))?;
            if o.rc != 0 {
                bail!("{}", o.stderr.trim());
            }
        }
        if let Some(m) = &dmode
            && !ctx.check
            && !exists
        {
            ctx.conn.exec(&format!("chmod {} {}", q(m), q(d)))?;
        }
        Ok(!exists)
    };
    changed |= mk(&top)?;
    for (rel, is_dir) in &entries {
        let target = format!("{top}/{rel}");
        if *is_dir {
            changed |= mk(&target)?;
            continue;
        }
        let data = match &local_root {
            Some(root) => crate::vault::read_file(&root.join(rel))?,
            None => ctx
                .conn
                .get_bytes(&format!("{}/{}", src.trim_end_matches('/'), rel))?
                .ok_or_else(|| anyhow!("remote src {src}/{rel} not found"))?,
        };
        let r = put_content(a, ctx, &target, &data)?;
        changed |= r.get("changed").and_then(Value::as_bool).unwrap_or(false);
    }
    let mut r = res(changed);
    r.insert("dest".into(), dest.into());
    r.insert("src".into(), src.into());
    r.insert("state".into(), "directory".into());
    if let Some(m) = dmode {
        r.insert("mode".into(), m.into());
    }
    Ok(r)
}

/// `file` module diff: JSON before/after of the path state (what Ansible shows with --diff).
fn state_diff(path: &str, before: &str, after: &str) -> Value {
    let j = |st: &str| {
        crate::pyfmt::json_dumps(
            &serde_json::json!({"path": path, "state": st}),
            Some(4),
            true,
            true,
        ) + "\n"
    };
    serde_json::json!({"before": j(before), "after": j(after), "before_header": "before", "after_header": "after"})
}

pub(super) fn file(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"])
        .ok_or_else(|| anyhow!("missing required argument: path"))?;
    let state = st(a, "state").unwrap_or_else(|| "file".into());
    let exists = ctx
        .conn
        .exec(&format!("test -e {p} || test -L {p}", p = q(&path)))?
        .rc
        == 0;
    let is_dir = ctx.conn.exec(&format!("test -d {}", q(&path)))?.rc == 0;
    let mut changed = false;
    let sh = |cmd: String| -> Result<()> {
        let o = ctx.conn.exec(&cmd)?;
        if o.rc != 0 {
            bail!("{}", o.stderr.trim());
        }
        Ok(())
    };
    match state.as_str() {
        "absent" => {
            if exists {
                changed = true;
                if !ctx.check {
                    sh(format!("rm -rf {}", q(&path)))?;
                }
            }
            let mut r = res(changed);
            r.insert("path".into(), path.clone().into());
            r.insert("state".into(), "absent".into());
            if changed && ctx.diff {
                r.insert(
                    "diff".into(),
                    state_diff(&path, if is_dir { "directory" } else { "file" }, "absent"),
                );
            }
            return Ok(r);
        }
        "directory" => {
            if !is_dir {
                changed = true;
                if !ctx.check {
                    sh(format!("mkdir -p {}", q(&path)))?;
                }
            }
        }
        "touch" => {
            changed = true;
            if !ctx.check {
                sh(format!("touch {}", q(&path)))?;
            }
        }
        "link" | "hard" => {
            let src = st(a, "src").ok_or_else(|| anyhow!("src is required for state=link"))?;
            let cur = ctx.conn.exec(&format!("readlink {}", q(&path)))?;
            if state == "link" && cur.rc == 0 && cur.stdout.trim() == src {
            } else {
                changed = true;
                if !ctx.check {
                    let flag = if state == "link" { "-sfn" } else { "-f" };
                    sh(format!("ln {flag} {} {}", q(&src), q(&path)))?;
                }
            }
        }
        _ => {
            if !exists {
                return Ok(fail(format!(
                    "file ({path}) is absent, cannot continue; use state=touch or state=directory"
                )));
            }
        }
    }
    let ac = if exists || changed {
        apply_attrs(a, ctx, &path)?
    } else {
        false
    };
    let mut r = res(changed || ac);
    if changed
        && ctx.diff
        && !exists
        && matches!(state.as_str(), "directory" | "touch" | "link" | "hard")
    {
        r.insert(
            "diff".into(),
            state_diff(
                &path,
                "absent",
                if state == "touch" {
                    "file"
                } else {
                    state.as_str()
                },
            ),
        );
    }
    r.insert("path".into(), path.clone().into());
    r.insert("state".into(), state.clone().into());
    if !ctx.check || exists {
        add_file_facts(ctx, &mut r, &path);
    }
    if state == "link" {
        r.insert("state".into(), "link".into());
        if let Some(src) = st(a, "src") {
            r.insert("dest".into(), path.clone().into());
            r.insert("src".into(), src.into());
        }
    }
    Ok(r)
}

pub(crate) fn read_text(ctx: &Ctx, path: &str, create: bool) -> Result<Option<String>> {
    match ctx.conn.get_bytes(path)? {
        Some(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
        None if create => Ok(None),
        None => bail!("Destination {path} does not exist !"),
    }
}

/// Marks a result as coming from lineinfile/blockinfile (they report `backup`, not `backup_file`).
fn lb(mut r: Res) -> Res {
    r.insert("__lb".into(), true.into());
    r
}

pub(crate) fn write_text(
    a: &Args,
    ctx: &Ctx,
    path: &str,
    old: Option<&str>,
    new: &str,
    mut r: Res,
) -> Result<Res> {
    let changed = old != Some(new);
    if changed {
        if ctx.diff {
            r.insert("diff".into(), diff_entry(old, new, path, true));
        }
        if !ctx.check {
            ctx.conn.exec(&format!("mkdir -p {}", q(&dir_of(path))))?;
            if let Some(f) = validate_content(a, ctx, new.as_bytes(), true)? {
                return Ok(f);
            }
            if old.is_some()
                && let Some(b) = make_backup(a, ctx, path)?
            {
                let key = if r.contains_key("__lb") {
                    "backup"
                } else {
                    "backup_file"
                };
                r.insert(key.into(), b.into());
            }
            ctx.conn.put(path, new.as_bytes(), None)?;
        }
    }
    r.remove("__lb");
    let ac = apply_attrs(a, ctx, path)?;
    r.insert("changed".into(), (changed || ac).into());
    if !ctx.check || old.is_some() {
        add_file_facts(ctx, &mut r, path);
    }
    Ok(r)
}

pub(super) fn lineinfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let create = bo(a, "create", false);
    let old = read_text(ctx, &path, create)?;
    let text = old.clone().unwrap_or_default();
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let line = st(a, "line");
    let re = st(a, "regexp")
        .map(|r| regex::Regex::new(&r))
        .transpose()
        .map_err(|e| anyhow!("bad regexp: {e}"))?;
    let mut msg = String::new();
    if state == "absent" {
        let before = ls.len();
        ls.retain(|l| match (&re, &line) {
            (Some(r), _) => !r.is_match(l),
            (None, Some(x)) => l != x,
            _ => true,
        });
        if ls.len() != before {
            msg = format!("{} line(s) removed", before - ls.len());
        }
    } else {
        let line =
            line.ok_or_else(|| anyhow!("one of line or regexp is required with state=present"))?;
        let backrefs = bo(a, "backrefs", false);
        let found = match &re {
            Some(r) => {
                let idxs: Vec<usize> = ls
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| r.is_match(l))
                    .map(|(i, _)| i)
                    .collect();
                if bo(a, "firstmatch", false) {
                    idxs.first().copied()
                } else {
                    idxs.last().copied()
                }
            }
            None => ls.iter().position(|l| *l == line),
        };
        match found {
            Some(i) => {
                let newl = if backrefs {
                    re.as_ref()
                        .unwrap()
                        .replace(&ls[i], crate::template::py_repl(&line).as_str())
                        .into_owned()
                } else {
                    line.clone()
                };
                if ls[i] != newl {
                    ls[i] = newl;
                    msg = "line replaced".into();
                }
            }
            None if backrefs => {}
            None => {
                let mut pos = ls.len();
                if let Some(b) = st(a, "insertbefore") {
                    if b == "BOF" {
                        pos = 0;
                    } else if let Ok(r) = regex::Regex::new(&b)
                        && let Some(i) = ls.iter().rposition(|l| r.is_match(l))
                    {
                        pos = i;
                    }
                } else if let Some(af) = st(a, "insertafter")
                    && af != "EOF"
                    && let Ok(r) = regex::Regex::new(&af)
                    && let Some(i) = ls.iter().rposition(|l| r.is_match(l))
                {
                    pos = i + 1;
                }
                ls.insert(pos, line);
                msg = "line added".into();
            }
        }
    }
    let mut new = ls.join("\n");
    if !ls.is_empty() {
        new.push('\n');
    }
    let mut r = res(false);
    r.insert("msg".into(), msg.into());
    write_text(a, ctx, &path, old.as_deref(), &new, lb(r))
}

pub(super) fn blockinfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let old = read_text(ctx, &path, bo(a, "create", false))?;
    let text = old.clone().unwrap_or_default();
    let marker = st(a, "marker").unwrap_or_else(|| "# {mark} ANSIBLE MANAGED BLOCK".into());
    let begin = marker.replace(
        "{mark}",
        &st(a, "marker_begin").unwrap_or_else(|| "BEGIN".into()),
    );
    let end = marker.replace(
        "{mark}",
        &st(a, "marker_end").unwrap_or_else(|| "END".into()),
    );
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    let bi = ls.iter().position(|l| *l == begin);
    let ei = bi.and_then(|b| ls.iter().skip(b).position(|l| *l == end).map(|e| e + b));
    let absent = st(a, "state").as_deref() == Some("absent");
    let block = st(a, "block").unwrap_or_default();
    let mut newblock: Vec<String> = vec![begin.clone()];
    newblock.extend(block.lines().map(String::from));
    newblock.push(end);
    match (bi, ei) {
        (Some(b), Some(e)) => {
            ls.splice(b..=e, if absent { vec![] } else { newblock });
        }
        _ if absent => {}
        _ => {
            let pos = match st(a, "insertbefore").as_deref() {
                Some("BOF") => 0,
                _ => ls.len(),
            };
            ls.splice(pos..pos, newblock);
        }
    }
    let mut new = ls.join("\n");
    if !ls.is_empty() {
        new.push('\n');
    }
    let mut r0 = res(false);
    let msg = if old.is_none() {
        "File created"
    } else if old.as_deref() == Some(new.as_str()) {
        ""
    } else if absent {
        "Block removed"
    } else {
        "Block inserted"
    };
    r0.insert("msg".into(), msg.into());
    write_text(a, ctx, &path, old.as_deref(), &new, lb(r0))
}

pub(super) fn replace(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let old = read_text(ctx, &path, false)?.unwrap_or_default();
    let re = regex::RegexBuilder::new(&st(a, "regexp").ok_or_else(|| anyhow!("missing regexp"))?)
        .multi_line(true)
        .build()?;
    let rep = crate::template::py_repl(&st(a, "replace").unwrap_or_default());
    let (after, before) = (st(a, "after"), st(a, "before"));
    let (count, new) = if after.is_some() || before.is_some() {
        let pat = match (&after, &before) {
            (Some(x), Some(y)) => format!("(?s){x}(?P<sub>.*?){y}"),
            (Some(x), None) => format!("(?s){x}(?P<sub>.*$)"),
            (None, Some(y)) => format!("(?s)^(?P<sub>.*?){y}"),
            _ => unreachable!(),
        };
        let sect = regex::Regex::new(&pat)?;
        match sect.captures(&old).and_then(|c| c.name("sub")) {
            Some(m) => {
                let part = &old[m.start()..m.end()];
                let n = re.find_iter(part).count();
                let rep_part = re.replace_all(part, rep.as_str());
                (
                    n,
                    format!("{}{}{}", &old[..m.start()], rep_part, &old[m.end()..]),
                )
            }
            None => (0, old.clone()),
        }
    } else {
        (
            re.find_iter(&old).count(),
            re.replace_all(&old, rep.as_str()).into_owned(),
        )
    };
    let mut r0 = res(false);
    r0.insert(
        "msg".into(),
        if new != old && count > 0 {
            format!("{count} replacements made")
        } else {
            String::new()
        }
        .into(),
    );
    write_text(a, ctx, &path, Some(&old), &new, r0)
}

pub(super) fn stat(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest"]).ok_or_else(|| anyhow!("missing path"))?;
    let p = q(&path);
    let algo = st(a, "checksum_algorithm").unwrap_or_else(|| "sha1".into());
    let sum_cmd = match algo.as_str() {
        "md5" => "md5sum \"$p\" 2>/dev/null || md5 -r \"$p\"",
        "sha224" => "sha224sum \"$p\" 2>/dev/null || shasum -a 224 \"$p\"",
        "sha256" => "sha256sum \"$p\" 2>/dev/null || shasum -a 256 \"$p\"",
        "sha384" => "sha384sum \"$p\" 2>/dev/null || shasum -a 384 \"$p\"",
        "sha512" => "sha512sum \"$p\" 2>/dev/null || shasum -a 512 \"$p\"",
        _ => "sha1sum \"$p\" 2>/dev/null || shasum \"$p\"",
    };
    let want_sum = bo(a, "get_checksum", true);
    let script = format!(
        r#"p={p}
if [ -L "$p" ]; then echo islnk=1; echo lnk_source=$(readlink "$p"); else echo islnk=0; fi
if [ -e "$p" ]; then echo exists=1
[ -d "$p" ] && echo isdir=1 || echo isdir=0
[ -f "$p" ] && echo isreg=1 || echo isreg=0
echo size=$(wc -c < "$p" 2>/dev/null | tr -d ' ')
echo mode=$(stat -L -c %a "$p" 2>/dev/null || stat -L -f %Lp "$p")
echo pw_name=$(stat -L -c %U "$p" 2>/dev/null || stat -L -f %Su "$p")
echo gr_name=$(stat -L -c %G "$p" 2>/dev/null || stat -L -f %Sg "$p")
echo uid=$(stat -L -c %u "$p" 2>/dev/null || stat -L -f %u "$p")
echo gid=$(stat -L -c %g "$p" 2>/dev/null || stat -L -f %g "$p")
echo mtime=$(stat -L -c %Y "$p" 2>/dev/null || stat -L -f %m "$p")
echo atime=$(stat -L -c %X "$p" 2>/dev/null || stat -L -f %a "$p")
echo ctime=$(stat -L -c %Z "$p" 2>/dev/null || stat -L -f %c "$p")
echo inode=$(stat -L -c %i "$p" 2>/dev/null || stat -L -f %i "$p")
echo nlink=$(stat -L -c %h "$p" 2>/dev/null || stat -L -f %l "$p")
[ -r "$p" ] && echo readable=1 || echo readable=0
[ -w "$p" ] && echo writeable=1 || echo writeable=0
[ -x "$p" ] && echo executable=1 || echo executable=0
{checksum_line}
else echo exists=0; fi"#,
        checksum_line = if want_sum {
            format!("[ -f \"$p\" ] && echo checksum=$( ({sum_cmd}) | cut -d' ' -f1)")
        } else {
            String::new()
        }
    );
    let o = ctx.conn.exec(&script)?;
    let mut s = Map::new();
    for l in o.stdout.lines() {
        if let Some((k, v)) = l.split_once('=') {
            let val = match k {
                "exists" | "isdir" | "isreg" | "islnk" | "readable" | "writeable"
                | "executable" => Value::Bool(v == "1"),
                "size" | "mtime" | "atime" | "ctime" | "uid" | "gid" | "inode" | "nlink" => {
                    v.parse::<i64>().map(Value::from).unwrap_or(Value::Null)
                }
                "mode" => Value::String(format!("{:0>4}", v)),
                _ => Value::String(v.to_string()),
            };
            s.insert(k.to_string(), val);
        }
    }
    if s.get("exists") != Some(&Value::Bool(true)) {
        s.clear();
        s.insert("exists".into(), false.into());
    } else {
        if bo(a, "get_mime", true) {
            let m = ctx
                .conn
                .exec(&format!("file -L --mime-type -b {p} 2>/dev/null"))?;
            let enc = ctx
                .conn
                .exec(&format!("file -L --mime-encoding -b {p} 2>/dev/null"))?;
            if m.rc == 0 && !m.stdout.trim().is_empty() {
                s.insert("mimetype".into(), m.stdout.trim().into());
                s.insert("charset".into(), enc.stdout.trim().into());
            }
        }
        s.insert("path".into(), path.clone().into());
        s.insert("isblk".into(), false.into());
        s.insert("ischr".into(), false.into());
        s.insert("isfifo".into(), false.into());
        s.insert("issock".into(), false.into());
    }
    let mut r = res(false);
    r.insert("stat".into(), Value::Object(s));
    Ok(r)
}

pub(super) fn slurp(a: &Args, ctx: &Ctx) -> Result<Res> {
    use base64::Engine;
    let path = st_any(a, &["src", "path"]).ok_or_else(|| anyhow!("missing src"))?;
    match ctx.conn.get_bytes(&path)? {
        Some(b) => {
            let mut r = res(false);
            r.insert(
                "content".into(),
                base64::engine::general_purpose::STANDARD.encode(b).into(),
            );
            r.insert("encoding".into(), "base64".into());
            r.insert("source".into(), path.into());
            Ok(r)
        }
        None => Ok(fail(format!("file not found: {path}"))),
    }
}

pub(super) fn include_vars(a: &Args, ctx: &Ctx) -> Result<Res> {
    let f = st_any(a, &["file", "_raw_params"]);
    let mut out = Map::new();
    if let Some(f) = f {
        let p = find_src(ctx, "vars", &f)?;
        crate::inventory::try_load_vars_file(&p, &mut out)?;
    } else if let Some(d) = st(a, "dir") {
        let p = find_src(ctx, "vars", &d)?;
        let mut fs: Vec<_> = std::fs::read_dir(&p)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yml" | "yaml" | "json")
                )
            })
            .collect();
        fs.sort();
        for f in fs {
            crate::inventory::try_load_vars_file(&f, &mut out)?;
        }
    }
    let out = match st(a, "name") {
        Some(n) => Map::from_iter([(n, Value::Object(out))]),
        None => out,
    };
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(out));
    Ok(r)
}

// ---------- control ----------

pub(super) fn unarchive(a: &Args, ctx: &Ctx) -> Result<Res> {
    let src = st(a, "src").ok_or_else(|| anyhow!("missing src"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let remote = bo(a, "remote_src", false);
    if let Some(c) = st(a, "creates")
        && ctx.conn.exec(&format!("test -e {}", q(&c)))?.rc == 0
    {
        let mut r = res(false);
        r.insert("skipped".into(), true.into());
        return Ok(r);
    }
    if ctx.check {
        return Ok(res(true));
    }
    let flag = if src.ends_with(".zip") {
        ""
    } else if src.ends_with(".tar.gz") || src.ends_with(".tgz") {
        "z"
    } else if src.ends_with(".tar.bz2") {
        "j"
    } else if src.ends_with(".tar.xz") {
        "J"
    } else {
        ""
    };
    let mut opts: Vec<String> = list(a, "extra_opts").iter().map(|o| q(o)).collect();
    opts.extend(
        list(a, "exclude")
            .iter()
            .map(|e| q(&format!("--exclude={e}"))),
    );
    let strip = opts.join(" ");
    if ctx.conn.exec(&format!("test -d {}", q(&dest)))?.rc != 0 {
        return Ok(fail(format!("dest '{dest}' must be an existing dir")));
    }
    let (data, path);
    let is_url =
        src.starts_with("http://") || src.starts_with("https://") || src.starts_with("ftp://");
    let mut downloaded: Option<String> = None;
    if is_url {
        // remote URL: fetch it onto the target first (like Ansible's unarchive does)
        let tmp = ctx
            .conn
            .exec("mktemp /tmp/.ransible-unarchive.XXXXXX")?
            .stdout
            .trim()
            .to_string();
        let o = ctx
            .conn
            .exec(&format!("curl -fsSL -o {} {}", q(&tmp), q(&src)))?;
        if o.rc != 0 {
            let _ = ctx.conn.exec(&format!("rm -f {}", q(&tmp)));
            return Ok(fail(format!(
                "Failed to download {src}: {}",
                o.stderr.trim()
            )));
        }
        data = None;
        path = tmp.clone();
        downloaded = Some(tmp);
    } else if remote {
        data = None;
        path = src.clone();
    } else {
        let p = find_src(ctx, "files", &src)?;
        data = Some(std::fs::read(&p)?);
        path = String::new();
    }
    // GNU tar: `--diff` against the destination tells whether extraction would change anything
    let gnu =
        !src.ends_with(".zip") && ctx.conn.exec("tar --version 2>/dev/null | grep -q GNU")?.rc == 0;
    let mut files: Option<Vec<String>> = None;
    if gnu && (remote || is_url) {
        if bo(a, "list_files", false) {
            let l = ctx
                .conn
                .exec(&format!("tar -t{flag}f {} {strip}", q(&path)))?;
            files = Some(l.stdout.lines().map(String::from).collect());
        }
        let d = ctx.conn.exec(&format!(
            "tar --diff -{flag}f {} -C {} {strip} 2>&1 | head -1",
            q(&path),
            q(&dest)
        ))?;
        if d.stdout.trim().is_empty() {
            if let Some(t) = downloaded {
                let _ = ctx.conn.exec(&format!("rm -f {}", q(&t)));
            }
            let mut r = res(false);
            r.insert("dest".into(), dest.clone().into());
            r.insert("src".into(), src.clone().into());
            r.insert("handler".into(), "TgzArchive".into());
            if let Some(f) = files {
                r.insert("files".into(), f.into());
            }
            return Ok(r);
        }
    }
    let cmd = if src.ends_with(".zip") {
        if remote {
            format!("unzip -o -q {} -d {}", q(&path), q(&dest))
        } else {
            format!(
                "cat > /tmp/.ransible.zip && unzip -o -q /tmp/.ransible.zip -d {} && rm -f /tmp/.ransible.zip",
                q(&dest)
            )
        }
    } else if remote {
        format!("tar -x{flag}f {} -C {} {strip}", q(&path), q(&dest))
    } else {
        format!("tar -x{flag}f - -C {} {strip}", q(&dest))
    };
    let o = match data {
        Some(d) => ctx.conn.exec_stdin(&cmd, &d)?,
        None => ctx.conn.exec(&cmd)?,
    };
    if let Some(t) = downloaded {
        let _ = ctx.conn.exec(&format!("rm -f {}", q(&t)));
    }
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    let mut r = res(true);
    r.insert("dest".into(), dest.clone().into());
    r.insert("src".into(), src.clone().into());
    if let Some(f) = files {
        r.insert("files".into(), f.into());
    }
    let _ = apply_attrs(a, ctx, &dest)?;
    Ok(r)
}

pub(super) fn tempfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let state = st(a, "state").unwrap_or_else(|| "file".into());
    let suffix = st(a, "suffix").unwrap_or_default();
    let prefix = st(a, "prefix").unwrap_or_else(|| "ansible.".into());
    let base = st(a, "path").unwrap_or_else(|| "/tmp".into());
    let tmpl = format!("{}/{}XXXXXX{}", base.trim_end_matches('/'), prefix, suffix);
    let o = ctx.conn.exec(&format!(
        "mktemp {} {}",
        if state == "directory" { "-d" } else { "" },
        q(&tmpl)
    ))?;
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    let mut r = res(true);
    r.insert("path".into(), o.stdout.trim().into());
    Ok(r)
}

pub(super) fn find(a: &Args, ctx: &Ctx) -> Result<Res> {
    let paths = list(a, "paths");
    let patterns = list(a, "patterns");
    let ftype = st(a, "file_type").unwrap_or_else(|| "file".into());
    let use_regex = bo(a, "use_regex", false);
    let depth = a.get("depth").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    });
    let mut skipped = Map::new();
    let mut cmd = String::new();
    let mut all_cmd = String::new();
    for p in &paths {
        if ctx.conn.exec(&format!("test -d {}", q(p)))?.rc != 0 {
            skipped.insert(
                p.clone(),
                format!("Skipped '{p}' path due to this access issue: '{p}' is not a directory")
                    .into(),
            );
            continue;
        }
        let mut base = format!("find {} -mindepth 1", q(p));
        match (depth, bo(a, "recurse", false)) {
            (Some(d), _) => base += &format!(" -maxdepth {d}"),
            (None, false) => base += " -maxdepth 1",
            _ => {}
        }
        all_cmd += &format!("{base} -print0 2>/dev/null; ");
        let mut c = base;
        match ftype.as_str() {
            "file" => c += " -type f",
            "directory" => c += " -type d",
            "link" => c += " -type l",
            _ => {}
        }
        if let Some(sz) = st(a, "size") {
            let sz = sz.trim().to_lowercase();
            let (sign, rest) = match sz.strip_prefix('-') {
                Some(r) => ('-', r.to_string()),
                None => ('+', sz.trim_start_matches('+').to_string()),
            };
            let (num, unit) = rest.split_at(
                rest.find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len()),
            );
            let mult: i64 = match unit {
                "k" | "kb" => 1024,
                "m" | "mb" => 1024 * 1024,
                "g" | "gb" => 1024 * 1024 * 1024,
                "t" | "tb" => 1024_i64.pow(4),
                _ => 1,
            };
            let n = num.parse::<i64>().unwrap_or(0) * mult;
            c += " -type f";
            c += &if sign == '+' {
                format!(" -size +{}c", (n - 1).max(0))
            } else {
                format!(" -size -{}c", n + 1)
            };
        }
        if let Some(age) = st(a, "age") {
            let (neg, rest) = if let Some(r) = age.strip_prefix('-') {
                (true, r.to_string())
            } else {
                (false, age.clone())
            };
            let (n, unit) = rest.split_at(rest.len().saturating_sub(1));
            let secs: i64 = n.parse::<i64>().unwrap_or(0)
                * match unit {
                    "m" => 60,
                    "h" => 3600,
                    "d" => 86400,
                    "w" => 604800,
                    _ => 1,
                };
            c += &format!(" -newermt '-{secs} seconds'").replace(
                " -newermt '-",
                if neg {
                    " -newermt '-"
                } else {
                    " ! -newermt '-"
                },
            );
        }
        c += " -print0";
        cmd += &format!("{c} 2>/dev/null; ");
    }
    let o = if cmd.is_empty() {
        Default::default()
    } else {
        ctx.conn.exec(&cmd)?
    };
    let examined = if all_cmd.is_empty() {
        0
    } else {
        ctx.conn
            .exec(&all_cmd)?
            .stdout
            .split('\0')
            .filter(|x| !x.is_empty())
            .count()
    };
    let re_list: Vec<regex::Regex> = patterns
        .iter()
        .filter_map(|p| {
            let pat = if use_regex {
                p.clone()
            } else {
                format!(
                    "^{}$",
                    regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
                )
            };
            regex::Regex::new(&pat).ok()
        })
        .collect();
    let excl: Vec<regex::Regex> = list(a, "excludes")
        .iter()
        .filter_map(|p| {
            regex::Regex::new(&format!(
                "^{}$",
                regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
            ))
            .ok()
        })
        .collect();
    let hidden = bo(a, "hidden", false);
    let mut files = vec![];
    for p in o.stdout.split('\0').filter(|x| !x.is_empty()) {
        let base = p.rsplit('/').next().unwrap_or(p);
        if !hidden && base.starts_with('.') {
            continue;
        }
        if !re_list.is_empty() && !re_list.iter().any(|r| r.is_match(base)) {
            continue;
        }
        if excl.iter().any(|r| r.is_match(base)) {
            continue;
        }
        if let Some(c) = st(a, "contains")
            && (ftype != "file"
                || ctx
                    .conn
                    .exec(&format!(
                        "grep -qE {} {}",
                        q(&c.replace("\\s", "[[:space:]]")),
                        q(p)
                    ))?
                    .rc
                    != 0)
        {
            continue;
        }
        let info = attrs(ctx.conn, p);
        files.push(json!({"path": p, "mode": info.as_ref().map(|i| format!("{:0>4}", i.0)), "pw_name": info.as_ref().map(|i| i.1.clone()), "gr_name": info.as_ref().map(|i| i.2.clone()), "isdir": ftype == "directory"}));
    }
    files.sort_by(|x, y| x["path"].as_str().cmp(&y["path"].as_str()));
    let mut r = res(false);
    r.insert("matched".into(), files.len().into());
    r.insert("examined".into(), examined.into());
    r.insert("files".into(), Value::Array(files));
    r.insert(
        "msg".into(),
        (if skipped.is_empty() {
            "All paths examined"
        } else {
            "Not all paths examined, check warnings for details"
        })
        .into(),
    );
    r.insert("skipped_paths".into(), Value::Object(skipped));
    Ok(r)
}
