# -*- coding: utf-8 -*-
"""紧凑版 JSON 序列化：小对象/短数组保持单行，只有超宽才折行。

`json.dumps(indent=2)` 会把每个数组元素、每个对象字段都拆成一行，
persona.json 这种「短字段 + 短数组」很多的结构会变得非常碎。
这里按 printWidth 决定：整体放得下就单行，放不下才逐项展开。
"""

import json
import unicodedata

# 显示宽度上限：中日韩字符按 2 列计，避免中文行看起来过长
DEFAULT_WIDTH = 110
DEFAULT_INDENT = 2


def _scalar(value) -> str:
    return json.dumps(value, ensure_ascii=False)


def _display_width(text: str) -> int:
    return sum(2 if unicodedata.east_asian_width(ch) in "WF" else 1 for ch in text)


def _inline(value) -> str:
    """把任意子树压成一行（不带换行）"""
    if isinstance(value, dict):
        if not value:
            return "{}"
        return "{" + ", ".join(f"{_scalar(k)}: {_inline(v)}" for k, v in value.items()) + "}"
    if isinstance(value, list):
        if not value:
            return "[]"
        return "[" + ", ".join(_inline(v) for v in value) + "]"
    return _scalar(value)


def dumps(obj, width: int = DEFAULT_WIDTH, indent: int = DEFAULT_INDENT) -> str:
    """序列化为「紧凑优先」的 JSON 文本（末尾带换行）"""

    def render(value, level: int) -> str:
        pad = " " * (indent * level)
        child_pad = " " * (indent * (level + 1))
        if isinstance(value, dict):
            if not value:
                return "{}"
            one_line = _inline(value)
            if _display_width(pad + one_line) <= width:
                return one_line
            items = [f"{_scalar(k)}: {render(v, level + 1)}" for k, v in value.items()]
            return "{\n" + ",\n".join(child_pad + item for item in items) + "\n" + pad + "}"
        if isinstance(value, list):
            if not value:
                return "[]"
            one_line = _inline(value)
            if _display_width(pad + one_line) <= width:
                return one_line
            return (
                "[\n"
                + ",\n".join(child_pad + render(item, level + 1) for item in value)
                + "\n"
                + pad
                + "]"
            )
        return _scalar(value)

    return render(obj, 0) + "\n"
