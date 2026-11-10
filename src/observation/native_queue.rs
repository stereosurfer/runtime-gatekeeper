//! Demand-driven GET, restricted to an explicitly registered path on a verified listener.
//! Retain only the last successful timestamp. Every returned count comes from this read.
use super::{metric, now};
use crate::{
    model::{QueueFormat, QueueSource},
    platform::{ProcessInstance, current_listener},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, time::Duration};

const LIMIT: u64 = 256 * 1024;

fn counts(format: QueueFormat, doc: &Value) -> Option<Value> {
    let (running, pending) = match format {
        QueueFormat::Counts => (doc["running"].as_u64()?, doc["pending"].as_u64()?),
        QueueFormat::ComfyQueue => (
            doc["queue_running"].as_array()?.len() as u64,
            doc["queue_pending"].as_array()?.len() as u64,
        ),
    };
    Some(json!({"running": running, "pending": pending}))
}

fn fetch(port: u16, source: &QueueSource) -> Result<Value, &'static str> {
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(Duration::from_millis(600))
        .build();
    let response = match agent
        .get(&format!("http://127.0.0.1:{port}{}", source.path))
        .call()
    {
        Ok(response) if response.status() == 200 => response,
        Ok(_) | Err(ureq::Error::Status(..)) => return Err("unexpected_http_status"),
        Err(_) => return Err("transport_error"),
    };
    let mut data = Vec::new();
    response
        .into_reader()
        .take(LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|_| "transport_error")?;
    if data.len() as u64 > LIMIT {
        return Err("response_too_large");
    }
    let doc = serde_json::from_slice(&data).map_err(|_| "invalid_json")?;
    counts(source.format, &doc).ok_or("invalid_queue_shape")
}

#[derive(Default)]
pub(crate) struct QueueReader {
    // One entry per configured service, local to this daemon.
    last_success: BTreeMap<String, (ProcessInstance, u64)>,
}

pub(super) fn observe(service: &Value, enabled: bool, reader: Option<&mut QueueReader>) -> Value {
    let mut out = metric(
        Value::Null,
        "loopback_http_get",
        "job_counts",
        &Value::Null,
        false,
        "unknown",
    );
    out["scope"] = json!("service_instance");
    out["work_order_attribution"] = json!("unknown");
    out["instance"] = Value::Null;
    out["registered_source"] = if service["queue_source"].is_null() {
        out["source"] = json!("registry:no_queue_source");
        Value::Null
    } else {
        json!({"method":"GET","host":"127.0.0.1","port":service["port"],
            "path": service["queue_source"]["path"], "format":service["queue_source"]["format"]})
    };
    out["last_success_at"] = Value::Null;
    match read(service, enabled, reader, &mut out) {
        Ok(value) => {
            out["value"] = value;
            out["availability"] = json!("available");
            out["confidence"] = json!("endpoint_reported");
            out["attribution_reason"] = json!("counts_only");
        }
        Err(reason) => out["reason"] = json!(reason),
    }
    out
}

fn read(
    service: &Value,
    enabled: bool,
    reader: Option<&mut QueueReader>,
    out: &mut Value,
) -> Result<Value, &'static str> {
    if service["queue_source"].is_null() {
        return Err("no_configured_source");
    }
    if !enabled {
        return Err("requires_targeted_fresh_snapshot");
    }
    if service["identity_verified"] != true || service["healthy"] != true {
        return Err("unverified_service");
    }
    let expected: ProcessInstance = serde_json::from_value(service["queue_instance"].clone())
        .map_err(|_| "listener_identity_unavailable")?;
    let reader = reader.ok_or("sampler_busy")?;
    let source: QueueSource =
        serde_json::from_value(service["queue_source"].clone()).map_err(|_| "invalid_source")?;
    let port = service["port"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or("invalid_source")?;
    let id = service["id"].as_str().ok_or("invalid_source")?;
    let validate = || {
        let current = current_listener(port).ok_or("listener_identity_unavailable")?;
        if current != expected {
            return Err("listener_instance_changed");
        }
        Ok(())
    };
    validate()?;
    let result = fetch(port, &source);
    let at = now();
    validate()?;
    out["instance"] = json!(expected);
    out["timestamp"] = json!(at);
    if result.is_ok() {
        reader
            .last_success
            .insert(id.to_owned(), (expected.clone(), at));
    }
    if let Some((_, last)) = reader
        .last_success
        .get(id)
        .filter(|(instance, _)| *instance == expected)
    {
        out["last_success_at"] = json!(last);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_formats_preserve_zero_and_redact_payloads() {
        assert_eq!(
            counts(
                QueueFormat::ComfyQueue,
                &json!({"queue_running":[{"prompt":"private"}],"queue_pending":[]})
            ),
            Some(json!({"running":1,"pending":0}))
        );
        assert_eq!(
            counts(
                QueueFormat::Counts,
                &json!({"running":0,"pending":0,"owner":"untrusted"})
            ),
            Some(json!({"running":0,"pending":0}))
        );
        for value in [json!(-1), json!(0.5), json!(true), json!("0"), Value::Null] {
            assert!(counts(QueueFormat::Counts, &json!({"running":value,"pending":0})).is_none());
        }
        assert!(counts(QueueFormat::ComfyQueue, &json!({"queue_running":[]})).is_none());
    }
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn same_pid_with_different_birth_marker_cannot_read_queue() {
    use crate::platform::process_instance;
    let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut wrong = process_instance(std::process::id()).unwrap();
    wrong.birth_marker ^= 1;
    let service = json!({"id":"reused-pid","queue_instance":wrong,"port": socket.local_addr().unwrap().port(),
        "identity_verified":true,"healthy":true,"queue_source":{"format":"counts","path":"/queue"}});
    let result = observe(&service, true, Some(&mut QueueReader::default()));
    assert_eq!(result["reason"], "listener_instance_changed");
    assert!(result["value"].is_null() && result["instance"].is_null());
    socket.set_nonblocking(true).unwrap();
    assert_eq!(
        socket.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
