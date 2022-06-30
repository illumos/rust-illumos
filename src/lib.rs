use anyhow::{bail, Result};
use log::{debug, error, info};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

mod os;
mod unix;

static SVCCFG_BIN: &str = "/usr/sbin/svccfg";
static SVCPROP_BIN: &str = "/usr/bin/svcprop";
static DEVPROP_BIN: &str = "/sbin/devprop";

fn spawn_reader<T>(
    name: &str,
    stream: Option<T>,
) -> Option<std::thread::JoinHandle<()>>
where
    T: Read + Send + 'static,
{
    let name = name.to_string();
    let stream = match stream {
        Some(stream) => stream,
        None => return None,
    };

    Some(std::thread::spawn(move || {
        let mut r = BufReader::new(stream);

        loop {
            let mut buf = String::new();

            match r.read_line(&mut buf) {
                Ok(0) => {
                    /*
                     * EOF.
                     */
                    return;
                }
                Ok(_) => {
                    let s = buf.trim();

                    if !s.is_empty() {
                        info!(target: "illumos-rs", "{}| {}", name, s);
                    }
                }
                Err(e) => {
                    error!(target: "illumos-rs", "failed to read {}: {}", name, e);
                    std::process::exit(100);
                }
            }
        }
    }))
}

pub fn devprop<S: AsRef<str>>(key: S) -> Result<String> {
    let key = key.as_ref();
    let val = run_capture_stdout(vec![DEVPROP_BIN, key].as_ref(), None)?;
    let lines: Vec<_> = val.lines().collect();
    if lines.len() != 1 {
        bail!("unexpected output for devprop {}: {:?}", key, lines);
    }
    Ok(lines[0].trim().to_string())
}

pub fn svccfg<S: AsRef<str>>(args: &[S], alt_root: Option<S>) -> Result<()> {
    let svccfg: Vec<&str> = vec![SVCCFG_BIN];
    let env = if let Some(alt_root) = alt_root {
        let alt_root = alt_root.as_ref();
        let dtd_path =
            format!("{}/usr/share/lib/xml/dtd/service_bundle.dtd.1", alt_root);
        let repo_path = format!("{}/etc/svc/repository.db", alt_root);
        let configd_path = format!("{}/lib/svc/bin/svc.configd", alt_root);
        Some(
            vec![
                ("SVCCFG_CHECKHASH", "1"),
                ("PKG_INSTALL_ROOT", alt_root),
                ("SVCCFG_DTD", &dtd_path),
                ("SVCCFG_REPOSITORY", &repo_path),
                ("SVCCFG_CONFIGD_PATH", &configd_path),
            ]
            .as_ref(),
        )
    } else {
        None
    };
    let mut stdin = String::new();
    for arg in args {
        let arg = arg.as_ref();
        stdin += &format!("{}\n", arg)
    }

    run_with_stdin(&svccfg, env, stdin)
}

pub fn svcprop(fmri: &str, prop_val: &str) -> Result<String> {
    let val = run_capture_stdout(
        vec![SVCPROP_BIN, "-p", prop_val, fmri].as_ref(),
        None,
    )?;
    let lines: Vec<_> = val.lines().collect();
    if lines.len() != 1 {
        bail!("unexpected output for svcprop {}: {:?}", fmri, lines);
    }
    Ok(lines[0].trim().to_string())
}

pub fn run_with_stdin<S: AsRef<str>>(
    args: &[S],
    env: Option<&[(S, S)]>,
    stdin: String,
) -> Result<()> {
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    let env = build_env(env);
    let mut cmd = build_cmd(args, env);

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let mut child_stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        child_stdin.write_all(stdin.as_bytes()).unwrap();
    });

    let readout = spawn_reader("O", child.stdout.take());
    let readerr = spawn_reader("E", child.stderr.take());

    if let Some(t) = readout {
        t.join().expect("join stdout thread");
    }
    if let Some(t) = readerr {
        t.join().expect("join stderr thread");
    }

    match child.wait() {
        Err(e) => Err(e.into()),
        Ok(es) => {
            if !es.success() {
                bail!("exec {:?}: failed {:?}", &args, &es)
            } else {
                Ok(())
            }
        }
    }
}

pub fn run<S: AsRef<str>>(args: &[S], env: Option<&[(S, S)]>) -> Result<()> {
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    let env = build_env(env);
    let mut cmd = build_cmd(args, env);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let readout = spawn_reader("O", child.stdout.take());
    let readerr = spawn_reader("E", child.stderr.take());

    if let Some(t) = readout {
        t.join().expect("join stdout thread");
    }
    if let Some(t) = readerr {
        t.join().expect("join stderr thread");
    }

    match child.wait() {
        Err(e) => Err(e.into()),
        Ok(es) => {
            if !es.success() {
                bail!("exec {:?}: failed {:?}", &args, &es)
            } else {
                Ok(())
            }
        }
    }
}

pub fn run_capture_stdout<S: AsRef<str>>(
    args: &[S],
    env: Option<&[(S, S)]>,
) -> Result<String> {
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    let env = build_env(env);
    let mut cmd = build_cmd(args, env);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?)
    } else {
        bail!(
            "exec {:?}: failed {:?}",
            &args,
            String::from_utf8(output.stderr)?
        )
    }
}

fn build_env<S: AsRef<str>>(
    env: Option<&[(S, S)]>,
) -> Option<Vec<(&str, &str)>> {
    if let Some(env) = env {
        let env: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect();
        Some(env)
    } else {
        None
    }
}

fn build_cmd(args: Vec<&str>, env: Option<Vec<(&str, &str)>>) -> Command {
    let mut cmd = Command::new(&args[0]);
    cmd.env_remove("LANG");
    cmd.env_remove("LC_CTYPE");
    cmd.env_remove("LC_NUMERIC");
    cmd.env_remove("LC_TIME");
    cmd.env_remove("LC_COLLATE");
    cmd.env_remove("LC_MONETARY");
    cmd.env_remove("LC_MESSAGES");
    cmd.env_remove("LC_ALL");

    if args.len() > 1 {
        cmd.args(&args[1..]);
    }

    if let Some(env) = env {
        cmd.envs(env);
        debug!(target: "illumos-rs", "exec: {:?} env={:?}", &args, &env);
    } else {
        debug!(target: "illumos-rs", "exec: {:?}", &args);
    }
    cmd
}
