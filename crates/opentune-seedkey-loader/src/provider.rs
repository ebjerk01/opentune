//! The external provider program: the runtime boundary that keeps every
//! seed/key algorithm out of OpenTune.
//!
//! The user installs a program that implements the algorithms their
//! definitions name, and points OpenTune at it by **absolute path**. For each
//! key OpenTune runs it directly (no shell, no `PATH` lookup, no arguments),
//! writes one JSON request to its stdin, closes stdin, and reads one JSON
//! response from its stdout:
//!
//! ```json
//! {"protocol":"opentune-seedkey-provider/1","provider":"MyAlgo","ecu":"MY-ECU",
//!  "access_level":17,"seed":"11223344","seed_length":4,"key_length":4,
//!  "parameters":[{"key":"Mask","type":"ByteArray","value":"0102"},
//!                {"key":"Rounds","type":"Int32","value":3}]}
//! ```
//!
//! answered by `{"key":"A1B2C3D4"}` (exit status 0) or `{"error":"why"}`.
//! The program has [`DEFAULT_TIMEOUT`] (configurable up to [`MAX_TIMEOUT`]) to
//! exit; it is killed after that. Stdout over [`MAX_OUTPUT`] bytes, output
//! that is not one such JSON object, a key of the wrong length or a non-zero
//! exit are errors. Stderr is drained and discarded (bounded) and never shown,
//! and no error message echoes the program's stdout, so a key or a secret
//! parameter is never printed by OpenTune.
use crate::defs::{to_hex, Definition, ParamValue};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The protocol tag sent in every request.
pub const PROTOCOL: &str = "opentune-seedkey-provider/1";
/// Default time the program has to answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest configurable timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Most stdout bytes read from the program.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// Longest provider error message passed on (characters).
pub const MAX_ERROR_LEN: usize = 200;

/// A user-installed provider program (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalProvider {
    program: PathBuf,
    timeout: Duration,
}

impl ExternalProvider {
    /// Check and wrap `program`: it must be an absolute path to an existing
    /// regular file; `timeout` must be between 1 ms and [`MAX_TIMEOUT`].
    pub fn new(program: PathBuf, timeout: Duration) -> Result<Self, String> {
        if !program.is_absolute() {
            return Err(format!(
                "provider program '{}' must be an absolute path (no PATH lookup is done)",
                program.display()
            ));
        }
        let meta = std::fs::metadata(&program)
            .map_err(|e| format!("provider program {}: {e}", program.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "provider program {}: not a regular file",
                program.display()
            ));
        }
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err(format!(
                "provider timeout must be between 1 ms and {} s",
                MAX_TIMEOUT.as_secs()
            ));
        }
        Ok(Self { program, timeout })
    }

    /// The program's path.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Ask the program for the key of `seed` under definition `def`.
    pub fn compute(&self, def: &Definition, seed: &[u8], level: u8) -> Result<Vec<u8>, String> {
        let mut body = serde_json::to_vec(&request(def, seed, level))
            .map_err(|e| format!("encode provider request: {e}"))?;
        body.push(b'\n');
        let name = self.program.display().to_string();
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("start provider program {name}: {e}"))?;

        // Feed stdin, drain stdout (bounded) and stderr (bounded, discarded)
        // on their own threads so a program that does not read, or writes a
        // lot, cannot block us past the deadline.
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = stdin.write_all(&body);
            });
        }
        let (tx, rx) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let r = out.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
                let _ = tx.send(r.map(|_| buf));
            });
        }
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut err.take(MAX_OUTPUT as u64), &mut std::io::sink());
            });
        }

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "provider program {name} did not answer within {} ms and was stopped",
                        self.timeout.as_millis()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    let _ = child.kill();
                    return Err(format!("provider program {name}: {e}"));
                }
            }
        };
        // The program has exited; its stdout closes unless it left a child
        // process holding it, so wait only briefly for the reader.
        let out = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) => return Err(format!("read provider program output: {e}")),
            Err(_) => {
                return Err(format!(
                    "provider program {name} exited but its output did not close"
                ))
            }
        };
        if out.len() > MAX_OUTPUT {
            return Err(format!(
                "provider program {name} wrote more than {MAX_OUTPUT} bytes; refused"
            ));
        }
        let resp = response(&out);
        if !status.success() {
            let code = status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| format!("status {c}"));
            return Err(match resp {
                Ok(Response::Error(msg)) => {
                    format!("provider program {name} failed ({code}): {msg}")
                }
                _ => format!("provider program {name} exited with {code}"),
            });
        }
        match resp? {
            Response::Error(msg) => Err(format!("provider program {name}: {msg}")),
            Response::Key(key) if key.len() != def.key_length => Err(format!(
                "provider program {name} returned a {}-byte key; the definition says KeyLength {}",
                key.len(),
                def.key_length
            )),
            Response::Key(key) => Ok(key),
        }
    }
}

/// The JSON request for `def` and `seed` at `level` (see the module docs).
pub fn request(def: &Definition, seed: &[u8], level: u8) -> Value {
    let params: Vec<Value> = def
        .parameters
        .iter()
        .map(|p| {
            let value = match &p.value {
                ParamValue::ByteArray(b) => json!(to_hex(b)),
                ParamValue::Int32(n) => json!(n),
                ParamValue::String(s) => json!(s),
            };
            json!({"key": p.key, "type": p.value.type_name(), "value": value})
        })
        .collect();
    json!({
        "protocol": PROTOCOL,
        "provider": def.provider,
        "ecu": def.ecu_name,
        "access_level": level,
        "seed": to_hex(seed),
        "seed_length": def.seed_length,
        "key_length": def.key_length,
        "parameters": params,
    })
}

/// A parsed provider response.
#[derive(PartialEq, Eq)]
enum Response {
    Key(Vec<u8>),
    Error(String),
}

/// Parse stdout. Errors describe the shape only, never the content.
fn response(out: &[u8]) -> Result<Response, String> {
    let garbage = || {
        format!(
            "provider program output ({} bytes) is not a JSON object with a hex \"key\" or an \"error\"",
            out.len()
        )
    };
    let text = std::str::from_utf8(out).map_err(|_| garbage())?;
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| garbage())?;
    let o = v.as_object().ok_or_else(garbage)?;
    match (o.get("key"), o.get("error")) {
        (Some(Value::String(k)), None) => crate::defs::parse_hex(k)
            .ok()
            .filter(|k| !k.is_empty())
            .map(Response::Key)
            .ok_or_else(garbage),
        (None, Some(Value::String(e))) => Ok(Response::Error(
            e.chars()
                .take(MAX_ERROR_LEN)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        )),
        _ => Err(garbage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::DefinitionSet;

    #[test]
    fn request_shape() {
        let set = DefinitionSet::parse(
            r#"[{"EcuName":"SIM","AccessLevel":17,"SeedLength":2,"KeyLength":2,"Provider":"DemoXor",
                "Parameters":[{"Key":"Mask","Value":"0a0B","DataType":"ByteArray"},
                              {"Key":"N","Value":-5,"DataType":"Int32"},
                              {"Key":"S","Value":"txt","DataType":"String"}]}]"#,
        )
        .unwrap();
        let r = request(&set.definitions[0], &[0x12, 0xAB], 0x11);
        assert_eq!(r["protocol"], PROTOCOL);
        assert_eq!(r["provider"], "DemoXor");
        assert_eq!(r["ecu"], "SIM");
        assert_eq!(r["access_level"], 17);
        assert_eq!(r["seed"], "12AB");
        assert_eq!(r["key_length"], 2);
        assert_eq!(r["parameters"][0]["value"], "0A0B");
        assert_eq!(r["parameters"][1]["value"], -5);
        assert_eq!(r["parameters"][2]["type"], "String");
    }

    #[test]
    fn responses() {
        assert!(matches!(response(b" {\"key\":\"0102\"}\n"), Ok(Response::Key(k)) if k == [1, 2]));
        assert!(
            matches!(response(b"{\"error\":\"no\\nway\"}"), Ok(Response::Error(e)) if e == "no way")
        );
        for bad in [
            &b""[..],
            b"0102",
            b"[\"0102\"]",
            b"{}",
            b"{\"key\":\"\"}",
            b"{\"key\":\"XYZ1\"}",
            b"{\"key\":258}",
            b"{\"key\":\"01\",\"error\":\"x\"}",
            b"\xff\xfe",
        ] {
            let e = response(bad).err().unwrap_or_default();
            assert!(e.contains("not a JSON object"), "{bad:?}: {e}");
        }
        // The garbage message never echoes the output.
        let e = response(b"{\"key\":\"SECRETKEY\"}").err().unwrap();
        assert!(!e.contains("SECRET"), "{e}");
    }

    #[test]
    fn program_paths_are_checked() {
        let e = ExternalProvider::new(PathBuf::from("provider"), DEFAULT_TIMEOUT).unwrap_err();
        assert!(e.contains("absolute"), "{e}");
        let missing = std::env::temp_dir().join("ot-skl-no-such-provider");
        assert!(ExternalProvider::new(missing, DEFAULT_TIMEOUT).is_err());
        let dir = std::env::temp_dir();
        assert!(ExternalProvider::new(dir, DEFAULT_TIMEOUT)
            .unwrap_err()
            .contains("not a regular file"));
        let exe = std::env::current_exe().unwrap();
        assert!(ExternalProvider::new(exe.clone(), Duration::ZERO).is_err());
        assert!(ExternalProvider::new(exe.clone(), MAX_TIMEOUT + Duration::from_secs(1)).is_err());
        assert_eq!(
            ExternalProvider::new(exe.clone(), DEFAULT_TIMEOUT)
                .unwrap()
                .program(),
            exe
        );
    }
}
