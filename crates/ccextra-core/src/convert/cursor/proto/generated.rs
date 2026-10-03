// prost 生成物只承担基础消息编解码。业务层使用 raw_wire，避免 oneof 丢混合字段。
include!(concat!(env!("OUT_DIR"), "/agent.v1.rs"));
