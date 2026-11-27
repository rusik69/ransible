use std::process::Command;

fn run(args: &[&str], out: &str) -> (String, i32) {
    let _ = std::fs::remove_dir_all(out);
    let o = Command::new(env!("CARGO_BIN_EXE_ransible"))
        .args(args)
        .env("RANSIBLE_OUT", out)
        .env("NO_COLOR", "1")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        o.status.code().unwrap_or(-1),
    )
}

#[test]
fn playbook_roles_blocks_handlers() {
    let out = "/tmp/ransible-it-1";
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/playbook.yml",
            "-i",
            "tests/fixtures/inventory.ini",
        ],
        out,
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("RUNNING HANDLER [reload web]"));
    assert!(stdout.contains("hi from web2 6"));
    let conf = std::fs::read_to_string(format!("{out}/web1/site.conf")).unwrap();
    assert!(
        conf.contains("listen 8081;")
            && conf.contains("name SHOP;")
            && conf.contains("env production;"),
        "{conf}"
    );
}

#[test]
fn yaml_inventory_and_limit() {
    let out = "/tmp/ransible-it-2";
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/playbook.yml",
            "-i",
            "tests/fixtures/inventory.yml",
            "-l",
            "web1",
        ],
        out,
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("web1") && !stdout.contains("web2 "));
}

#[test]
fn tags_and_check_mode() {
    let out = "/tmp/ransible-it-3";
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/playbook.yml",
            "-i",
            "tests/fixtures/inventory.ini",
            "-t",
            "base",
            "--check",
        ],
        out,
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(!std::path::Path::new(&format!("{out}/web1")).exists());
    assert!(stdout.contains("changed: [web1]"));
}

#[test]
fn failure_sets_exit_code() {
    let out = "/tmp/ransible-it-4";
    let (_s, code) = run(
        &[
            "adhoc",
            "web1",
            "-i",
            "tests/fixtures/inventory.ini",
            "-m",
            "command",
            "-a",
            "false",
        ],
        out,
    );
    assert_eq!(code, 2);
}

#[test]
fn variable_precedence_matches_ansible() {
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/prec/pb.yml",
            "-i",
            "tests/fixtures/prec/inv.ini",
            "-e",
            "extra_v=extra",
        ],
        "/tmp/ransible-it-5",
    );
    assert_eq!(code, 0, "{stdout}");
    let got: Vec<String> = stdout
        .lines()
        .filter(|l| l.contains("\"msg\""))
        .map(|l| l.trim().to_string())
        .collect();
    let want: Vec<String> = std::fs::read_to_string("tests/fixtures/prec/expected.txt")
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(got, want);
}

fn msgs(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| l.contains("\"msg\""))
        .map(|l| l.trim().to_string())
        .collect()
}

fn expected(name: &str) -> Vec<String> {
    std::fs::read_to_string(format!("tests/fixtures/{name}.expected"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn add_host_and_group_by_match_ansible() {
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/dyn.yml",
            "-i",
            "tests/fixtures/inventory.ini",
        ],
        "/tmp/ransible-it-6",
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(msgs(&stdout), expected("dyn"));
}

#[test]
fn async_and_rescue_vars_match_ansible() {
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/flow.yml",
            "-i",
            "tests/fixtures/inventory.ini",
        ],
        "/tmp/ransible-it-7",
    );
    assert_eq!(code, 0, "{stdout}");
    let got: Vec<String> = msgs(&stdout)
        .into_iter()
        .filter(|l| !l.starts_with("fatal"))
        .collect();
    let want: Vec<String> = expected("flow")
        .into_iter()
        .filter(|l| !l.starts_with("fatal"))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn filters_match_ansible() {
    let (stdout, code) = run(
        &["playbook", "tests/fixtures/filters.yml"],
        "/tmp/ransible-it-8",
    );
    assert_eq!(code, 0, "{stdout}");
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let got = squash(&stdout[stdout.find("\"msg\"").unwrap()..stdout.rfind(']').unwrap() + 1]);
    let want = squash(&std::fs::read_to_string("tests/fixtures/filters.expected").unwrap());
    assert_eq!(got, want);
}

#[test]
fn free_strategy_does_not_wait_for_slow_hosts() {
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/free.yml",
            "-i",
            "tests/fixtures/inventory.ini",
        ],
        "/tmp/ransible-it-9",
    );
    assert_eq!(code, 0, "{stdout}");
    let p1 = stdout.find("after web1").expect("web1 output");
    let p2 = stdout.find("after web2").expect("web2 output");
    assert!(
        p2 < p1,
        "web2 should finish before the slow web1 under strategy: free\n{stdout}"
    );
    assert!(stdout.contains("web1") && stdout.contains("ok=2") && stdout.contains("web2"));
}

#[test]
fn linear_strategy_keeps_hosts_in_lockstep() {
    let src = std::fs::read_to_string("tests/fixtures/free.yml")
        .unwrap()
        .replace("  strategy: free\n", "");
    let tmp = std::env::temp_dir().join("ransible-linear.yml");
    std::fs::write(&tmp, src).unwrap();
    let (stdout, code) = run(
        &[
            "playbook",
            tmp.to_str().unwrap(),
            "-i",
            "tests/fixtures/inventory.ini",
        ],
        "/tmp/ransible-it-10",
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.find("after web1").unwrap() < stdout.find("after web2").unwrap());
}

#[test]
fn roles_from_collections_resolve_by_fqcn() {
    let dir = std::env::temp_dir().join(format!("ransible-coll-{}", std::process::id()));
    let role = dir.join("collections/ansible_collections/my/coll/roles/web/tasks");
    std::fs::create_dir_all(&role).unwrap();
    std::fs::write(
        role.join("main.yml"),
        "- debug: msg=\"from collection role\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("site.yml"), "- hosts: localhost\n  connection: local\n  gather_facts: no\n  roles:\n    - my.coll.web\n").unwrap();
    let (stdout, code) = run(
        &["playbook", dir.join("site.yml").to_str().unwrap()],
        "/tmp/ransible-it-11",
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("from collection role"), "{stdout}");
}

#[test]
fn vault_ids_select_password_by_label_and_missing_secret_is_an_error() {
    let dir = std::env::temp_dir().join(format!("ransible-vid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = env!("CARGO_BIN_EXE_ransible");
    let sh = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    std::fs::write(dir.join("dev.pw"), "devpw\n").unwrap();
    std::fs::write(dir.join("prod.pw"), "prodpw\n").unwrap();
    std::fs::write(dir.join("dev.yml"), "who_dev: d\n").unwrap();
    std::fs::write(dir.join("prod.yml"), "who_prod: p\n").unwrap();
    assert!(
        sh(&[
            "vault",
            "encrypt",
            "dev.yml",
            "--vault-id",
            "dev@dev.pw",
            "--encrypt-vault-id",
            "dev"
        ])
        .status
        .success()
    );
    assert!(
        sh(&[
            "vault",
            "encrypt",
            "prod.yml",
            "--vault-id",
            "dev@dev.pw",
            "--vault-id",
            "prod@prod.pw",
            "--encrypt-vault-id",
            "prod"
        ])
        .status
        .success()
    );
    assert!(
        std::fs::read_to_string(dir.join("prod.yml"))
            .unwrap()
            .starts_with("$ANSIBLE_VAULT;1.2;AES256;prod")
    );
    std::fs::write(dir.join("p.yml"), "- hosts: localhost\n  connection: local\n  gather_facts: no\n  vars_files: [dev.yml, prod.yml]\n  tasks:\n    - debug: msg=\"{{ who_dev }}{{ who_prod }}\"\n").unwrap();
    let ok = sh(&[
        "playbook",
        "p.yml",
        "--vault-id",
        "dev@dev.pw",
        "--vault-id",
        "prod@prod.pw",
    ]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(String::from_utf8_lossy(&ok.stdout).contains("\"dp\""));
    let missing = sh(&["playbook", "p.yml", "--vault-id", "dev@dev.pw"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Decryption failed"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run a compat fixture and compare msg + recap lines (order-insensitive) with real Ansible's output.
fn compat(name: &str) {
    let (stdout, code) = run(
        &[
            "playbook",
            &format!("tests/fixtures/compat/{name}.yml"),
            "-i",
            "tests/fixtures/compat/inv.ini",
        ],
        &format!("/tmp/ransible-it-compat-{name}"),
    );
    assert_eq!(code, 0, "{stdout}");
    let norm = |s: &str| -> String {
        let mut v: Vec<String> = s
            .lines()
            .filter(|l| {
                !l.starts_with("fatal:")
                    && (l.contains("\"msg\"")
                        || l.starts_with("w1 ")
                        || l.starts_with("w2 ")
                        || l.starts_with("d1 "))
            })
            .map(|l| {
                l.replace(" : ok=", " ok=")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        v.sort();
        v.join("\n")
    };
    let want = std::fs::read_to_string(format!("tests/fixtures/compat/{name}.expected")).unwrap();
    assert_eq!(
        norm(&stdout),
        norm(&want),
        "output differs from real Ansible for {name}"
    );
}

#[test]
fn compat_loops_lookups_filters_match_ansible() {
    compat("p1");
}

#[test]
fn compat_blocks_handlers_meta_match_ansible() {
    compat("p2");
}

#[test]
fn compat_templates_and_yaml_json_filters_are_byte_identical_to_ansible() {
    for f in ["out", "y", "j", "k"] {
        let _ = std::fs::remove_file(format!("/tmp/ransible-compat-p3-{f}.txt"));
    }
    let (stdout, code) = run(
        &[
            "playbook",
            "tests/fixtures/compat/p3.yml",
            "-i",
            "tests/fixtures/compat/inv.ini",
        ],
        "/tmp/ransible-it-compat-p3",
    );
    assert_eq!(code, 0, "{stdout}");
    for f in ["out", "y", "j", "k"] {
        let got = std::fs::read_to_string(format!("/tmp/ransible-compat-p3-{f}.txt")).unwrap();
        let want = std::fs::read_to_string(format!("tests/fixtures/compat/p3.expected.d/{f}.txt"))
            .unwrap();
        assert_eq!(got, want, "rendered file '{f}' differs from real Ansible's");
    }
}
