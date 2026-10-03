use super::CursorConvertError;
use prost::Message;
use prost_types::{value::Kind, ListValue, Struct, Value as ProtoValue};
use serde_json::Value;

fn to_proto(value: &Value) -> ProtoValue {
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(v) => Kind::BoolValue(*v),
        Value::Number(v) => Kind::NumberValue(v.as_f64().unwrap_or_default()),
        Value::String(v) => Kind::StringValue(v.clone()),
        Value::Array(values) => Kind::ListValue(ListValue {
            values: values.iter().map(to_proto).collect(),
        }),
        Value::Object(object) => Kind::StructValue(Struct {
            fields: object
                .iter()
                .map(|(key, value)| (key.clone(), to_proto(value)))
                .collect(),
        }),
    };
    ProtoValue { kind: Some(kind) }
}

pub(super) fn tools(
    body: &Value,
) -> Result<Vec<super::proto::generated::McpToolDefinition>, CursorConvertError> {
    use super::proto::generated::McpToolDefinition;
    if body.pointer("/tool_choice/type").and_then(Value::as_str) == Some("none") {
        return Ok(Vec::new());
    }
    if let Some(choice) = body.pointer("/tool_choice/type").and_then(Value::as_str) {
        if choice != "auto" {
            return Err(CursorConvertError::Unsupported(format!(
                "tool_choice={choice}"
            )));
        }
    }
    let items = body.get("tools").and_then(Value::as_array);
    let mut tools = Vec::new();
    for item in items.into_iter().flatten() {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CursorConvertError::Invalid("工具缺少 name".into()))?;
        let schema = item
            .get("input_schema")
            .ok_or_else(|| CursorConvertError::Invalid(format!("工具 {name} 缺少 input_schema")))?;
        tools.push(McpToolDefinition {
            name: name.into(),
            description: item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            input_schema: to_proto(schema).encode_to_vec(),
            provider_identifier: "proxy".into(),
            tool_name: name.into(),
        });
    }
    Ok(tools)
}

fn from_proto(value: ProtoValue) -> Value {
    match value.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(v)) => Value::Bool(v),
        Some(Kind::NumberValue(v)) => serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(v)) => Value::String(v),
        Some(Kind::ListValue(v)) => Value::Array(v.values.into_iter().map(from_proto).collect()),
        Some(Kind::StructValue(v)) => Value::Object(
            v.fields
                .into_iter()
                .map(|(key, item)| (key, from_proto(item)))
                .collect(),
        ),
    }
}

pub fn decode_mcp_args(
    args: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<Value, CursorConvertError> {
    let mut object = serde_json::Map::new();
    for (key, bytes) in args {
        let value = ProtoValue::decode(bytes.as_slice()).map_err(|err| {
            CursorConvertError::Invalid(format!("MCP 参数 {key} 解码失败: {err}"))
        })?;
        object.insert(key.clone(), from_proto(value));
    }
    Ok(Value::Object(object))
}
