use crate::engine::Engine;
use serde_json::{Value, json};

pub fn tools() -> Value {
    let names = [
        "runtime.status",
        "runtime.request",
        "runtime.release",
        "services.list",
        "services.get",
        "services.start",
        "services.stop",
        "services.restart",
        "jobs.list",
        "jobs.get",
        "runtime.cleanup",
    ];
    json!(names.iter().map(|name|{
        let (properties,required,description)=match *name {
            "runtime.request"=>(json!({"job_id":{"type":"string","minLength":1,"maxLength":128},"agent":{"type":"string","minLength":1,"maxLength":128},"requires":{"type":"array","items":{"type":"string"},"minItems":1},"memory_bytes":{"type":"integer","minimum":0,"description":"Additional job memory beyond service startup estimates, in bytes"}}),json!(["job_id","agent","requires"]),"Acquire a runtime lease. Returns READY, BLOCKED_RESOURCE or BLOCKED_SERVICE; never evicts work. Same job_id and request are idempotent."),
            "runtime.release"=>(json!({"runtime_id":{"type":"string"}}),json!(["runtime_id"]),"Release a runtime lease. Does not stop shared services."),
            "services.get"|"services.start"|"services.stop"|"services.restart"=>(json!({"service_id":{"type":"string"}}),json!(["service_id"]),"Inspect or control a configured service. Controls require managed ownership, allowed action, and no leases."),
            "jobs.get"=>(json!({"job_id":{"type":"string"}}),json!(["job_id"]),"Read one job and its runtime lease."),
            "runtime.cleanup"=>(json!({}),json!([]),"Stop only daemon-owned disposable services with no leases and allowed stop action."),
            _=>(json!({}),json!([]),"Read the current workstation registry and resource snapshot.")
        };
        json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":matches!(*name,"runtime.status"|"services.list"|"services.get"|"jobs.list"|"jobs.get"),"openWorldHint":false}})
    }).collect::<Vec<_>>())
}
pub fn rpc(engine: &mut Engine, v: Value) -> Option<Value> {
    let id = v.get("id").cloned();
    if v["jsonrpc"] != "2.0" || !v["method"].is_string() {
        return Some(error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
    }
    let method = v["method"].as_str().unwrap();
    let id = id?;
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":match v["params"]["protocolVersion"].as_str(){Some("2024-11-05")=>"2024-11-05",Some("2025-11-25")=>"2025-11-25",_=>"2025-06-18"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"runtime-gatekeeper","version":env!("CARGO_PKG_VERSION")},"instructions":"Use runtime.request once per job and runtime.release on completion. Report BLOCKED_RESOURCE to the human. Never evict other work."})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => {
            let name = v["params"]["name"].as_str().unwrap_or("");
            let args = v["params"].get("arguments").cloned().unwrap_or(json!({}));
            let catalog = tools();
            let Some(tool) = catalog
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
            else {
                return Some(error(id, -32602, "Unknown tool"));
            };
            let schema = &tool["inputSchema"];
            let valid = args.as_object().is_some_and(|a| {
                a.keys().all(|k| schema["properties"].get(k).is_some())
                    && schema["required"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|k| a.contains_key(k.as_str().unwrap()))
            });
            if !valid {
                return Some(error(id, -32602, "Invalid tool arguments"));
            }
            match engine.call(name, args) {
                Ok(result) => {
                    json!({"content":[{"type":"text","text":result.to_string()}],"structuredContent":result,"isError":false})
                }
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
pub fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
