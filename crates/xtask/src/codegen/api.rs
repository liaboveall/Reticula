//! Emits the browser client from the server's OpenAPI schemas and operations.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::BANNER;
use crate::util::Result;

const HTTP_CLIENT: &str = r#"export class ApiError extends Error {
  constructor(readonly status: number) {
    super(`Reticula API returned ${status}`);
  }
}

async function request(
  method: "GET" | "POST",
  path: string,
  signal?: AbortSignal,
  data?: unknown,
): Promise<Response> {
  const options: RequestInit = { method, credentials: "same-origin", redirect: "error" };
  if (signal) options.signal = signal;
  if (data !== undefined) {
    options.headers = { "Content-Type": "application/json" };
    options.body = JSON.stringify(data);
  }
  const response = await fetch(path, options);
  if (!response.ok) throw new ApiError(response.status);
  return response;
}
"#;

pub(super) fn typescript() -> Result<String> {
    let spec = serde_json::to_value(reticula_server::http::openapi()).map_err(|e| e.to_string())?;
    let mut out = String::from(BANNER);
    out.push('\n');
    for (name, schema) in map(&spec["components"]["schemas"])? {
        if let Some(properties) = schema.get("properties") {
            let _ = writeln!(out, "export interface {name} {{");
            for (key, property) in map(properties)? {
                let mark = if matches!(&schema["required"], Value::Array(keys) if keys.iter().any(|item| item == key.as_str()))
                {
                    ""
                } else {
                    "?"
                };
                let _ = writeln!(out, "  {key}{mark}: {};", type_of(property)?);
            }
            out.push_str("}\n\n");
        } else {
            let _ = writeln!(out, "export type {name} = {};\n", type_of(schema)?);
        }
    }
    out.push_str(HTTP_CLIENT);
    for (path, operations) in map(&spec["paths"])? {
        for (method, operation) in map(operations)? {
            operation_ts(&mut out, method, path, operation)?;
        }
    }
    Ok(out)
}

fn type_of(schema: &Value) -> Result<String> {
    if let Some(reference) = schema["$ref"].as_str() {
        return Ok(reference.rsplit('/').next().unwrap_or(reference).to_owned());
    }
    if let Some(values) = schema["enum"].as_array() {
        return Ok(values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | "));
    }
    if let Some(variants) = schema["oneOf"].as_array() {
        return Ok(variants
            .iter()
            .map(type_of)
            .collect::<Result<Vec<_>>>()?
            .join(" | "));
    }
    match &schema["type"] {
        Value::Array(types) => Ok(types
            .iter()
            .map(ts_primitive)
            .collect::<Result<Vec<_>>>()?
            .join(" | ")),
        kind if kind == "object" && schema.get("additionalProperties").is_some() => Ok(format!(
            "Record<string, {}>",
            type_of(&schema["additionalProperties"])?
        )),
        kind => ts_primitive(kind),
    }
}

fn ts_primitive(kind: &Value) -> Result<String> {
    match kind.as_str() {
        Some(kind @ ("string" | "boolean" | "null")) => Ok(kind.to_owned()),
        Some("integer" | "number") => Ok("number".to_owned()),
        _ => Err(format!("unsupported API schema type: {kind}")),
    }
}

fn operation_ts(out: &mut String, method: &str, path: &str, op: &Value) -> Result {
    let id = string(&op["operationId"], "operation ID")?;
    let mut parts = id.split('_');
    let mut name = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.push_str(chars.as_str());
        }
    }
    let responses = &op["responses"];
    let url_only = responses.get("101").is_some() || responses.get("303").is_some();
    let mut signature = Vec::new();
    let mut query = Vec::new();
    let mut route = path.to_owned();
    for parameter in op["parameters"].as_array().into_iter().flatten() {
        let name = string(&parameter["name"], "parameter name")?;
        match parameter["in"].as_str() {
            Some("path") => {
                signature.push(format!("{name}: {}", type_of(&parameter["schema"])?));
                route = route.replace(
                    &format!("{{{name}}}"),
                    &format!("${{encodeURIComponent({name})}}"),
                );
            }
            Some("query") if url_only => {
                let optional = parameter["required"].as_bool() != Some(true);
                signature.push(format!("{name}{}: string", if optional { "?" } else { "" }));
                query.push((name, optional));
            }
            _ => return Err(format!("unsupported parameters on {id}")),
        }
    }
    let route = if route.contains("${") {
        format!("`{route}`")
    } else {
        format!("\"{route}\"")
    };
    if url_only {
        let _ = writeln!(
            out,
            "\nexport function {name}Url({}): string {{",
            signature.join(", ")
        );
        if query.is_empty() {
            let _ = writeln!(out, "  return {route};");
        } else {
            out.push_str("  const parameters = new URLSearchParams();\n");
            for (key, optional) in query {
                let condition = if optional {
                    format!("if ({key} !== undefined) ")
                } else {
                    String::new()
                };
                let _ = writeln!(out, "  {condition}parameters.set(\"{key}\", {key});");
            }
            let _ = writeln!(
                out,
                "  const query = parameters.toString();\n  return query ? {route} + `?${{query}}` : {route};"
            );
        }
    } else {
        let data = if let Some(schema) = op.pointer("/requestBody/content/application~1json/schema")
        {
            signature.push(format!("body: {}", type_of(schema)?));
            ", body"
        } else {
            ""
        };
        signature.push("signal?: AbortSignal".into());
        let response = ["200", "201", "204"]
            .into_iter()
            .find_map(|status| responses.get(status))
            .ok_or_else(|| format!("missing success response for {id}"))?;
        let (ty, result) =
            if let Some(schema) = response.pointer("/content/application~1json/schema") {
                (type_of(schema)?, "  return response.json();\n")
            } else if response
                .pointer("/content/application~1octet-stream")
                .is_some()
            {
                (
                    "ArrayBuffer".to_owned(),
                    "  return response.arrayBuffer();\n",
                )
            } else {
                ("void".to_owned(), "")
            };
        let _ = writeln!(
            out,
            "\nexport async function {name}({}): Promise<{ty}> {{",
            signature.join(", ")
        );
        let binding = if result.is_empty() {
            ""
        } else {
            "const response = "
        };
        let _ = writeln!(
            out,
            "  {binding}await request(\"{}\", {route}, signal{data});",
            method.to_uppercase()
        );
        out.push_str(result);
    }
    out.push_str("}\n");
    Ok(())
}

fn map(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| "expected an OpenAPI object".to_owned())
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| format!("missing OpenAPI {field}"))
}
