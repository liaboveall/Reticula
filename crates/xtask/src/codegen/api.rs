//! Emits the browser client from the server's OpenAPI schemas and operations.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::BANNER;
use crate::util::Result;

const HTTP_CLIENT: &str = r#"export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly body?: unknown,
  ) {
    super(`Reticula API returned ${status}`);
  }
}

async function request(
  method: "GET" | "POST" | "PATCH" | "DELETE",
  path: string,
  signal?: AbortSignal,
  data?: unknown,
  extraHeaders?: Record<string, string>,
): Promise<Response> {
  const options: RequestInit = { method, credentials: "same-origin", redirect: "error" };
  if (signal) options.signal = signal;
  const headers: Record<string, string> = extraHeaders ? { ...extraHeaders } : {};
  if (data instanceof Uint8Array) {
    headers["Content-Type"] = "application/octet-stream";
    options.body = new Uint8Array(data);
  } else if (data !== undefined) {
    headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(data);
  }
  if (Object.keys(headers).length > 0) {
    options.headers = headers;
  }
  const response = await fetch(path, options);
  if (!response.ok) {
    let body: unknown;
    const ct = response.headers.get("content-type") ?? "";
    if (ct.includes("application/json")) {
      try {
        body = await response.json();
      } catch {
        body = undefined;
      }
    }
    throw new ApiError(response.status, body);
  }
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
    if let Some(variants) = schema["anyOf"].as_array() {
        return Ok(variants
            .iter()
            .map(type_of)
            .collect::<Result<Vec<_>>>()?
            .join(" | "));
    }
    if let Some(parts) = schema["allOf"].as_array() {
        let mut mapped = parts.iter().map(type_of).collect::<Result<Vec<_>>>()?;
        let base = if mapped.len() == 1 {
            mapped.pop().unwrap_or_else(|| "unknown".to_owned())
        } else {
            mapped.join(" & ")
        };
        if schema["nullable"].as_bool() == Some(true) && !base.contains("null") {
            return Ok(format!("{base} | null"));
        }
        return Ok(base);
    }
    match &schema["type"] {
        Value::Null => Ok("unknown".to_owned()),
        Value::Array(types) => Ok(types
            .iter()
            .map(ts_primitive)
            .collect::<Result<Vec<_>>>()?
            .join(" | ")),
        kind if kind == "array" => {
            let item = type_of(&schema["items"])?;
            if item.contains(" | ") {
                Ok(format!("Array<{item}>"))
            } else {
                Ok(format!("{item}[]"))
            }
        }
        kind if kind == "object" && schema.get("properties").is_some() => {
            let mut fields = Vec::new();
            for (key, prop) in map(&schema["properties"])? {
                let mark = if matches!(&schema["required"], Value::Array(keys) if keys.iter().any(|item| item == key.as_str()))
                {
                    ""
                } else {
                    "?"
                };
                fields.push(format!("{key}{mark}: {}", type_of(prop)?));
            }
            Ok(format!("{{ {} }}", fields.join("; ")))
        }
        kind if kind == "object" && schema.get("additionalProperties").is_some() => {
            let add = &schema["additionalProperties"];
            if add.is_boolean() {
                Ok("Record<string, unknown>".to_owned())
            } else {
                Ok(format!("Record<string, {}>", type_of(add)?))
            }
        }
        kind if kind == "object" => Ok("Record<string, unknown>".to_owned()),
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

fn param_ident(raw: &str) -> String {
    if !raw.contains(['_', '-']) && !raw.chars().next().is_some_and(char::is_uppercase) {
        return raw.to_owned();
    }
    let mut out = String::new();
    for (idx, seg) in raw.split(['_', '-']).filter(|s| !s.is_empty()).enumerate() {
        if idx == 0 {
            out.push_str(&seg.to_ascii_lowercase());
        } else {
            let mut chars = seg.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(&chars.as_str().to_ascii_lowercase());
            }
        }
    }
    out
}

fn non_null_type(schema: &Value) -> Result<String> {
    let raw = type_of(schema)?;
    let parts: Vec<&str> = raw.split(" | ").filter(|part| *part != "null").collect();
    if parts.is_empty() {
        Ok(raw)
    } else {
        Ok(parts.join(" | "))
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
    let url_only = responses.get("101").is_some()
        || responses.get("303").is_some()
        || responses
            .pointer("/200/content/text~1event-stream")
            .is_some();
    let mut signature = Vec::new();
    let mut query = Vec::new();
    let mut opt_query = Vec::new();
    let mut req_headers = Vec::new();
    let mut opt_headers = Vec::new();
    let mut route = path.to_owned();
    for parameter in op["parameters"].as_array().into_iter().flatten() {
        let raw_name = string(&parameter["name"], "parameter name")?;
        let ident = param_ident(raw_name);
        let optional = parameter["required"].as_bool() != Some(true);
        match parameter["in"].as_str() {
            Some("path") => {
                signature.push(format!("{ident}: {}", type_of(&parameter["schema"])?));
                route = route.replace(
                    &format!("{{{raw_name}}}"),
                    &format!("${{encodeURIComponent({ident})}}"),
                );
            }
            Some("query") if url_only => {
                signature.push(format!(
                    "{ident}{}: string",
                    if optional { "?" } else { "" }
                ));
                query.push((raw_name, ident, optional));
            }
            Some("query") if optional => {
                let ty = non_null_type(&parameter["schema"])?;
                opt_query.push((raw_name, ident, ty));
            }
            Some("header") if !optional => {
                let ty = non_null_type(&parameter["schema"])?;
                signature.push(format!("{ident}: {ty}"));
                req_headers.push((raw_name, ident));
            }
            Some("header") => {
                let ty = non_null_type(&parameter["schema"])?;
                opt_headers.push((raw_name, ident, ty));
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
            for (raw_key, ident, optional) in query {
                let condition = if optional {
                    format!("if ({ident} !== undefined) ")
                } else {
                    String::new()
                };
                let _ = writeln!(out, "  {condition}parameters.set(\"{raw_key}\", {ident});");
            }
            let _ = writeln!(
                out,
                "  const query = parameters.toString();\n  return query ? {route} + `?${{query}}` : {route};"
            );
        }
    } else {
        let has_headers = !req_headers.is_empty() || !opt_headers.is_empty();
        let data = if let Some(schema) = op.pointer("/requestBody/content/application~1json/schema")
        {
            signature.push(format!("body: {}", type_of(schema)?));
            Some("body")
        } else if op
            .pointer("/requestBody/content/application~1octet-stream")
            .is_some()
        {
            signature.push("body: Uint8Array".to_owned());
            Some("body")
        } else {
            None
        };
        signature.push("signal?: AbortSignal".into());
        if !opt_query.is_empty() || !opt_headers.is_empty() {
            let fields = opt_query
                .iter()
                .map(|(_, ident, ty)| format!("{ident}?: {ty}"))
                .chain(
                    opt_headers
                        .iter()
                        .map(|(_, ident, ty)| format!("{ident}?: {ty}")),
                )
                .collect::<Vec<_>>()
                .join("; ");
            signature.push(format!("options?: {{ {fields} }}"));
        }
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
        let target = if opt_query.is_empty() {
            route
        } else {
            out.push_str("  const parameters = new URLSearchParams();\n");
            for (raw_key, ident, _) in &opt_query {
                let _ = writeln!(
                    out,
                    "  if (options?.{ident} != null) parameters.set(\"{raw_key}\", String(options.{ident}));"
                );
            }
            out.push_str("  const query = parameters.toString();\n");
            let _ = writeln!(
                out,
                "  const target = query ? {route} + `?${{query}}` : {route};"
            );
            "target".to_owned()
        };
        if has_headers {
            out.push_str("  const headers: Record<string, string> = {};\n");
            for (raw_key, ident) in &req_headers {
                let _ = writeln!(out, "  headers[\"{raw_key}\"] = {ident};");
            }
            for (raw_key, ident, _) in &opt_headers {
                let _ = writeln!(
                    out,
                    "  if (options?.{ident} != null) headers[\"{raw_key}\"] = String(options.{ident});"
                );
            }
        }
        let binding = if result.is_empty() {
            ""
        } else {
            "const response = "
        };
        let trailing = match (data, has_headers) {
            (Some(body), true) => format!(", {body}, headers"),
            (Some(body), false) => format!(", {body}"),
            (None, true) => ", undefined, headers".to_owned(),
            (None, false) => String::new(),
        };
        let _ = writeln!(
            out,
            "  {binding}await request(\"{}\", {target}, signal{trailing});",
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
