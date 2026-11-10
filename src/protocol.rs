use anyhow::Result;
use serde_json::{Value, json};

fn tools(observer: bool) -> Value {
    let names = [
        "runtime.status",
        "runtime.observe",
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
    json!(names.iter().filter(|name| !observer || read_only(name)).map(|name|{
        let (properties,required,description)=match *name {
            "runtime.observe"=>(json!({"service_id":{"type":"string","minLength":1,"maxLength":128}}),json!([]),"Read resource evidence. For service activity, idle, unload or concurrency questions, pass the registered service_id to get native_queue running/pending, attributed memory, reservations and availability. This read never grants admission."),
            "runtime.request"=>(json!({"job_id":{"type":"string","minLength":1,"maxLength":128},"agent":{"type":"string","minLength":1,"maxLength":128},"requires":{"type":"array","items":{"type":"string"},"minItems":1},"memory_bytes":{"type":"integer","minimum":0,"description":"Measured additional job memory in bytes. Omit if unknown; READY then has memory_assurance=unknown."}}),json!(["job_id","agent","requires"]),"Acquire a runtime lease. Service dependencies expand automatically; exclusive groups block concurrent work. Returns READY, BLOCKED_RESOURCE or BLOCKED_SERVICE; never evicts work."),
            "runtime.release"=>(json!({"runtime_id":{"type":"string"}}),json!(["runtime_id"]),"Release a runtime lease. Does not stop shared services."),
            "services.get"=>(json!({"service_id":{"type":"string"}}),json!(["service_id"]),"Read registered service ownership, leases and process usage. For native activity use runtime.observe with service_id."),
            "services.start"|"services.stop"|"services.restart"=>(json!({"service_id":{"type":"string"}}),json!(["service_id"]),"Control a registered managed service with an allowed action and no protecting leases."),
            "jobs.get"=>(json!({"job_id":{"type":"string"}}),json!(["job_id"]),"Read one job and its runtime lease."),
            "runtime.cleanup"=>(json!({}),json!([]),"Stop only daemon-owned disposable services with no leases and allowed stop action."),
            "runtime.status"=>(json!({}),json!([]),"Read host and Gatekeeper registry status. The jobs array counts registered work orders only. Native queue and graphics memory are unavailable here. For an individual service use runtime.observe with service_id if listed."),
            "jobs.list"=>(json!({}),json!([]),"List work orders registered with Gatekeeper. An empty jobs array says nothing about the native engine queue. Read runtime.observe with service_id for native activity."),
            "services.list"=>(json!({}),json!([]),"List registered services, leases and process ownership. Read runtime.observe with service_id for a native queue sample."),
            _=>unreachable!("tool catalog is exhaustive")
        };
        json!({"name":name,"description":description,"outputSchema":output_schema(name),"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only(name),"openWorldHint":false}})
    }).collect::<Vec<_>>())
}
pub fn rpc(
    observer: bool,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
    v: Value,
) -> Option<Value> {
    let id = v.get("id").cloned();
    if v["jsonrpc"] != "2.0" || !v["method"].is_string() {
        return Some(error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
    }
    let method = v["method"].as_str().unwrap();
    let id = id?;
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":match v["params"]["protocolVersion"].as_str(){Some("2024-11-05")=>"2024-11-05",Some("2025-11-25")=>"2025-11-25",_=>"2025-06-18"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"runtime-gatekeeper","version":env!("CARGO_PKG_VERSION")},"instructions":if observer {"Read-only resource observation. Inspect evidence; refer service controls to a separately authorized work order."} else {"Use runtime.request once per job and runtime.release on completion. Report BLOCKED_RESOURCE to the human. Never evict other work."}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools(observer)}),
        "tools/call" => {
            let name = v["params"]["name"].as_str().unwrap_or("");
            let args = v["params"].get("arguments").cloned().unwrap_or(json!({}));
            let catalog = tools(observer);
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
                a.iter().all(|(k, v)| {
                    schema["properties"]
                        .get(k)
                        .is_some_and(|rule| valid_value(v, rule))
                }) && schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|k| a.contains_key(k.as_str().unwrap()))
            });
            if !valid {
                return Some(error(id, -32602, "Invalid tool arguments"));
            }
            match call(name, args) {
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

pub fn read_only(name: &str) -> bool {
    matches!(
        name,
        "runtime.observe"
            | "runtime.status"
            | "services.list"
            | "services.get"
            | "jobs.list"
            | "jobs.get"
    )
}
fn output_schema(name: &str) -> Value {
    let properties = match name {
        "runtime.observe" => {
            json!({"version":{"const":1},"observation_only":{"const":true},"snapshot":{"type":"object"},"system":{"type":"object"},"process_accounting":{"type":"object"},"reservations":{"type":"object"},"services":{"type":"array"}})
        }
        "runtime.status" => {
            json!({"resources":{"type":"object","required":["system","process_accounting"]},"snapshot":{"type":"object","required":["id","sampled_at","age_ms","stale"]},"memory":{"type":"object"},"services":{"type":"array"},"jobs":{"type":"array"},"unknown":{"type":"array"}})
        }
        "services.list" => json!({"services":{"type":"array"}}),
        "jobs.list" => json!({"jobs":{"type":"array"}}),
        "services.get" => json!({"id":{"type":"string"},"state":{"type":"string"}}),
        "jobs.get" => json!({"runtime_id":{"type":"string"},"status":{"type":"string"}}),
        "runtime.cleanup" => json!({"results":{"type":"array"}}),
        "runtime.request" => {
            return json!({"type":"object","required":["status"],"properties":{
            "status":{"type":"string"}, "runtime_id":{"type":"string"}, "resource":{"type":"string"},
            "required":{"type":["integer","null"]}, "available":{"type":["integer","null"]}, "shortfall":{"type":["integer","null"]},
            "reclaimable":{"type":"array"}, "protected":{"type":"array"}, "memory_assurance":{"type":"string"}
        },"additionalProperties":true});
        }
        _ => json!({"status":{"type":"string"}}),
    };
    let required: Vec<_> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":true})
}

fn valid_value(value: &Value, schema: &Value) -> bool {
    match schema["type"].as_str() {
        Some("string") => value.as_str().is_some_and(|s| {
            let len = s.chars().count() as u64;
            schema["minLength"].as_u64().is_none_or(|min| len >= min)
                && schema["maxLength"].as_u64().is_none_or(|max| len <= max)
        }),
        Some("integer") => value.as_u64().is_some(),
        Some("array") => value.as_array().is_some_and(|a| {
            schema["minItems"]
                .as_u64()
                .is_none_or(|min| a.len() as u64 >= min)
                && a.iter().all(|v| valid_value(v, &schema["items"]))
        }),
        _ => false,
    }
}
