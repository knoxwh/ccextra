use super::{ProviderConfig, RouteError};
use serde_json::Value;

#[derive(Clone, Copy)]
struct Variant<'a> {
    id: &'a str,
    effort: &'a str,
    thinking: bool,
    fast: bool,
}

fn variant(id: &str) -> (&str, Variant<'_>) {
    let mut base = id;
    let mut variant = Variant {
        id,
        effort: "",
        thinking: false,
        fast: false,
    };
    while let Some((prefix, suffix)) = base.rsplit_once('-') {
        match suffix {
            "fast" => variant.fast = true,
            "thinking" => variant.thinking = true,
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                if variant.effort.is_empty() =>
            {
                variant.effort = suffix;
                if suffix == "high" && prefix.ends_with("-extra") {
                    variant.effort = "xhigh";
                    base = &prefix[..prefix.len() - "-extra".len()];
                    continue;
                }
            }
            _ => break,
        }
        base = prefix;
    }
    (base, variant)
}

fn option<'a>(body: &'a Value, name: &str) -> Result<&'a str, RouteError> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(RouteError::InvalidOption(format!("{name} 必须是字符串"))),
    }
}

fn effort(body: &Value) -> Result<&str, RouteError> {
    if let Some(value) = body.pointer("/output_config/effort") {
        return value
            .as_str()
            .ok_or_else(|| RouteError::InvalidOption("output_config.effort 必须是字符串".into()));
    }
    if let Some(value) = body.pointer("/thinking/effort") {
        return value
            .as_str()
            .ok_or_else(|| RouteError::InvalidOption("thinking.effort 必须是字符串".into()));
    }
    option(body, "reasoning_effort")
}

fn fast(body: &Value) -> Result<bool, RouteError> {
    let mut fast = false;
    for key in ["service_tier", "speed"] {
        match option(body, key)? {
            "" | "auto" | "default" | "standard" => {}
            "fast" | "priority" => fast = true,
            value => return Err(RouteError::InvalidOption(format!("{key}={value}"))),
        }
    }
    Ok(fast)
}

pub(super) fn resolve_cursor_model(
    inbound: &str,
    body: &Value,
    provider: &ProviderConfig,
    matched: &str,
) -> Result<String, RouteError> {
    let model = if inbound == "auto" && !provider.models.iter().any(|m| m.name == "auto") {
        provider
            .metadata
            .as_ref()
            .and_then(|m| m.get("default_model"))
            .ok_or_else(|| {
                RouteError::VariantUnavailable("auto 未被广告且未配置 cursor_default_model".into())
            })?
            .as_str()
    } else if inbound != matched && provider.models.iter().any(|m| m.name == matched) {
        matched
    } else {
        inbound
    };
    let (base, _) = variant(model);
    let variants: Vec<_> = provider
        .models
        .iter()
        .filter_map(|m| {
            let (family, details) = variant(&m.name);
            (family == base).then_some(details)
        })
        .collect();
    if variants.is_empty() {
        return if inbound == "auto" {
            Err(RouteError::VariantUnavailable(format!(
                "cursor_default_model={model} 未被广告"
            )))
        } else {
            Err(RouteError::ModelNotFound(model.to_string()))
        };
    }
    if base != model {
        return variants
            .iter()
            .find(|v| v.id == model)
            .map(|v| v.id.to_string())
            .ok_or_else(|| RouteError::VariantUnavailable(format!("{model} 未被广告")));
    }

    let mut effort = effort(body)?.trim().to_ascii_lowercase();
    let thinking = match body.pointer("/thinking/type") {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value.as_str(),
        Some(_) => {
            return Err(RouteError::InvalidOption(
                "thinking.type 必须是字符串".into(),
            ))
        }
    };
    let fast = fast(body)?;
    let has_effort = variants.iter().any(|v| !v.effort.is_empty());
    let has_thinking = variants.iter().any(|v| v.thinking);
    if !has_effort && !has_thinking {
        effort.clear();
    }
    if effort == "auto" {
        effort.clear();
    }
    if !matches!(
        effort.as_str(),
        "" | "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Err(RouteError::InvalidOption(format!(
            "reasoning_effort={effort}"
        )));
    }
    if !matches!(thinking, "" | "disabled" | "enabled" | "adaptive" | "auto") {
        return Err(RouteError::InvalidOption(format!(
            "thinking.type={thinking}"
        )));
    }
    if !thinking.is_empty() && thinking != "disabled" && effort == "none" {
        return Err(RouteError::InvalidOption(
            "thinking 与 effort=none 冲突".into(),
        ));
    }
    if effort.is_empty() && thinking.is_empty() && !fast && variants.iter().any(|v| v.id == model) {
        return Ok(model.to_string());
    }
    let want_thinking = has_thinking && thinking != "disabled" && effort != "none";
    if thinking == "disabled" && !has_thinking {
        effort = "none".into();
    }
    let mut best: Option<(i32, &str)> = None;
    for v in variants {
        if v.fast != fast || v.thinking != want_thinking {
            continue;
        }
        if !effort.is_empty() && effort != "none" && v.effort != effort {
            continue;
        }
        if effort == "none" && !has_thinking && !matches!(v.effort, "" | "none") {
            continue;
        }
        let score = match v.effort {
            "medium" => 7,
            "high" => 6,
            "" => 5,
            "low" => 4,
            "minimal" => 3,
            "xhigh" => 2,
            "max" => 1,
            _ => 0,
        };
        if best.map_or(true, |(rank, id)| {
            score > rank || score == rank && v.id < id
        }) {
            best = Some((score, v.id));
        }
    }
    best.map(|(_, id)| id.to_string()).ok_or_else(|| {
        RouteError::VariantUnavailable(format!(
            "{model}: effort={effort:?}, thinking={want_thinking}, fast={fast} 无可用 variant"
        ))
    })
}
