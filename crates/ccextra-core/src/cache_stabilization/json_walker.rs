//! 用于跨 API 种类遍历 tools 和 messages 的 JSON walker 工具。
//!
//! 提供对 Anthropic 和 OpenAI 请求结构的统一迭代。

use serde_json::Value;

/// 请求体中的 tools 数组迭代器。
pub struct ToolWalker<'a> {
    tools: Option<&'a mut Vec<Value>>,
}

impl<'a> ToolWalker<'a> {
    /// 从请求体创建 walker。若不存在 tools 数组则返回 None。
    pub fn new(body: &'a mut Value) -> Option<Self> {
        let tools = body.get_mut("tools")?.as_array_mut()?;
        Some(Self { tools: Some(tools) })
    }

    /// 对每个 tool 应用函数，返回总数。
    pub fn for_each<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(&mut Value) -> usize,
    {
        let Some(tools) = self.tools.as_mut() else {
            return 0;
        };
        let mut count = 0;
        for tool in tools.iter_mut() {
            count += f(tool);
        }
        count
    }

    /// 获取 tools 数组的可变引用。
    pub fn tools_mut(&mut self) -> Option<&mut Vec<Value>> {
        self.tools.as_deref_mut()
    }
}
