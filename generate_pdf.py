#!/usr/bin/env python3
"""Convert BENCHMARK_RESULTS.md to styled HTML for PDF printing."""

import markdown

with open("BENCHMARK_RESULTS.md", "r") as f:
    md_content = f.read()

html_body = markdown.markdown(
    md_content,
    extensions=["tables", "fenced_code", "toc"],
)

html = f"""<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>ARC Chain — Benchmark Results & Technical Overview</title>
<style>
  @page {{
    size: A4;
    margin: 1.8cm 2cm;
  }}

  @media print {{
    body {{ font-size: 10px; }}
    h1 {{ font-size: 22px; }}
    h2 {{ font-size: 15px; break-after: avoid; }}
    h3 {{ font-size: 12px; break-after: avoid; }}
    table {{ break-inside: avoid; }}
    pre {{ break-inside: avoid; }}
  }}

  * {{
    box-sizing: border-box;
  }}

  body {{
    font-family: -apple-system, 'Helvetica Neue', Arial, sans-serif;
    font-size: 11px;
    line-height: 1.6;
    color: #1a1a2e;
    max-width: 800px;
    margin: 0 auto;
    padding: 20px 30px;
  }}

  h1 {{
    font-size: 26px;
    font-weight: 700;
    color: #03030A;
    border-bottom: 3px solid #002DDE;
    padding-bottom: 10px;
    margin-top: 0;
    margin-bottom: 4px;
  }}

  h2 {{
    font-size: 17px;
    font-weight: 700;
    color: #002DDE;
    border-bottom: 1.5px solid #E5E5EA;
    padding-bottom: 4px;
    margin-top: 30px;
    margin-bottom: 10px;
  }}

  h3 {{
    font-size: 13px;
    font-weight: 600;
    color: #3855E9;
    margin-top: 20px;
    margin-bottom: 8px;
  }}

  p {{
    margin: 6px 0;
  }}

  strong {{
    color: #03030A;
  }}

  table {{
    width: 100%;
    border-collapse: collapse;
    margin: 10px 0 18px 0;
    font-size: 10px;
  }}

  thead th {{
    background: #03030A;
    color: #FFFFFF;
    padding: 8px 10px;
    text-align: left;
    font-weight: 600;
    font-size: 10px;
    letter-spacing: 0.3px;
  }}

  tbody td {{
    padding: 6px 10px;
    border-bottom: 1px solid #E5E5EA;
    vertical-align: top;
  }}

  tbody tr:nth-child(even) {{
    background: #F8F8FA;
  }}

  code {{
    font-family: 'SF Mono', 'Fira Code', 'Consolas', monospace;
    font-size: 9.5px;
    background: #F0F0F5;
    padding: 1px 5px;
    border-radius: 3px;
    color: #002DDE;
  }}

  pre {{
    background: #0A2540;
    color: #E5E5EA;
    padding: 16px 20px;
    border-radius: 8px;
    font-size: 9.5px;
    line-height: 1.55;
    overflow-x: auto;
    margin: 12px 0;
  }}

  pre code {{
    background: none;
    color: #E5E5EA;
    padding: 0;
    font-size: 9.5px;
  }}

  blockquote {{
    border-left: 3px solid #6F7CF4;
    margin: 14px 0;
    padding: 10px 18px;
    background: #F4F4F8;
    font-size: 10px;
    color: #555;
    border-radius: 0 6px 6px 0;
  }}

  hr {{
    border: none;
    border-top: 1px solid #E5E5EA;
    margin: 24px 0;
  }}

  ul, ol {{
    margin: 6px 0;
    padding-left: 22px;
  }}

  li {{
    margin: 3px 0;
  }}
</style>
</head>
<body>
{html_body}
</body>
</html>"""

output_path = "ARC_Chain_Benchmark_Results.html"
with open(output_path, "w") as f:
    f.write(html)

print(f"HTML generated: {output_path}")
print(f"Full path: /Users/tjdunham/Desktop/ARC/arc-chain/{output_path}")
