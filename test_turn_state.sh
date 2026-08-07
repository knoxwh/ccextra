#!/bin/bash
# 测试:抓取上游响应头,验证是否返回 x-codex-turn-state

curl -i -X POST http://127.0.0.1:8222/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: your-secret" \
  -d '{
    "model": "gpt-5.6-terra",
    "max_tokens": 100,
    "messages": [{"role": "user", "content": "hi"}],
    "stream": false
  }' 2>&1 | grep -i "x-codex\|x-request\|openai-"
