#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""飞书群自定义机器人文本推送。机器人若开启「关键词」校验，正文需包含 keyword。

用法：
  python 飞书转发.py "摘要正文…"
  或：echo 摘要 | python 飞书转发.py -
"""
import sys
import requests

KEYWORD = "【机器人】"
WEBHOOK = "https://open.feishu.cn/open-apis/bot/v2/hook/646392d3-c205-41fc-b7c0-01a342eac341"
ACCESS_TOKEN = "tooilGCilgCxxhlub0Qtbb"


def post_message(body_text: str) -> None:
    text = body_text if KEYWORD in body_text else (KEYWORD + body_text)
    resp = requests.post(
        WEBHOOK,
        headers={
            "Authorization": ACCESS_TOKEN,
            "Content-Type": "application/json",
        },
        json={
            "msg_type": "text",
            "content": {"text": text},
        },
        timeout=15,
    )
    print(resp.text, flush=True)
    resp.raise_for_status()


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] != "-":
        msg = sys.argv[1]
    else:
        msg = sys.stdin.read()
    msg = (msg or "").strip()
    if not msg:
        print("摘要为空：请通过参数传入或通过 stdin 传入", file=sys.stderr)
        sys.exit(2)
    post_message(msg)
