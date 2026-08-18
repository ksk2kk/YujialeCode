# Third-party notices

> 这是法律归属文件，不参与编译或运行。学习时把它理解成项目的“借书登记册”：凡是
> 直接移植或派生的第三方实现，都应在这里保留来源与许可证，不能用代码注释代替。

YJLcoder 的系统提示词与品牌为独立实现。本项目的工具工程设计参考了以下
开源项目；固定提交号用于复现本轮设计审计。

## Claude Code Best（仅接口与交互设计参考）

- Repository: https://github.com/claude-code-best/claude-code
- Referenced commit: `3bb6b5746238c418138eb96d57765d79012edd96`
- Referenced area: `AskUserQuestionTool` 的公开输入/输出形态与交互流程；
  `FileReadTool`、`readFileInRange`、`addLineNumbers` 的范围/上限/输出语义；
  `FileReadTool/UI` 与 REPL 启动页的终端交互布局
- YJLcoder 使用独立 Rust 实现；未复制该项目的系统提示词或 TypeScript/React 源码。

## Exa MCP Server

- Repository: https://github.com/exa-labs/exa-mcp-server
- Referenced commit: `e64c11f2d3b4400ffbda8ccdd9658a450cc9d270`
- Copyright (c) 2025 Exa Labs
- License: MIT

## Pi Agent Harness

- Repository: https://github.com/earendil-works/pi
- Referenced commit: `9d2ec7ffabe927bfad2214c1cee25b6632a78dcf`
- Copyright (c) 2025 Mario Zechner
- License: MIT

## MIT License text

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
