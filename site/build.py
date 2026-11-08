from argparse import ArgumentParser
from html import escape
import json
from pathlib import Path
import re
import shutil
from urllib.parse import urlsplit
from xml.etree import ElementTree as ET

SOURCE = Path(__file__).resolve().parent
BENCH = "https://github.com/condense-chat/minmax-bench"
PIN = "5d6ff271d8b4ec7fa061b205815fca507377c835"
ARM = {"headroom": "Headroom · cache mode", "headroom-kompress": "Headroom · Kompress",
       "condense-sync": "Condense", "condense-async": "Condense"}
COLUMNS = ["Strategy", "Matched turns", "Baseline", "After", "Input tokens saved", "Model cost saved"]
RUNS = {
    "haiku-capped": ("Haiku 4.5 with a 190k context cap", "Three compression strategies on six SWE-chat sessions. Across its 1,600 matched turns, Condense saved 50.7% of input tokens and 28.0% of model cost.", "Each strategy is paired with its own successful baseline turns. The Headroom comparison also reports the 1,571 turns that all three completed."),
    "haiku-long": ("Haiku 4.5 with uncapped context", "On 2,909 matched turns, Condense saved 37.3% of model cost. The shortest prompts cost more, and long contexts drove the savings.", "The uncapped run followed the same six recorded histories further. On prompts under 16k, Condense increased model cost by 58.6%. At 400k and above it saved 53.4%. This run tested only Condense."),
    "opus-rewrite": ("Opus 4.8 across 64 coding sessions", "Across 11,712 turns, estimated model cost fell from $861.28 to $312.20 with Condense. Headroom cache mode raised it to $878.31.", "The harness rewrote recorded requests, simulated prompt caching and applied Opus 4.8 prices. These totals estimate the model bill for a fixed conversation. Agent decisions on a new trajectory and Condense service fees fall outside this calculation.")
}


def data(key):
    return json.loads((SOURCE.parent / "docs" / "data" / (key + ".json")).read_text())


def template(name):
    return (SOURCE / "pages" / (name + ".html")).read_text()


def fill(content, values):
    return re.sub(r"\{\{(\w+)\}\}", lambda m: values[m[1]], content)


def table(caption, headings, values):
    rows = []
    for name, *cells in values:
        style = ' class="focus"' if name == "Condense" else ""
        row = "".join(f"<td>{escape(value)}</td>" for value in cells)
        rows.append(f'<tr{style}><th scope="row">{escape(name)}</th>{row}</tr>')
    columns = "".join(f'<th scope="col">{escape(label)}</th>' for label in headings)
    return (f'<div class="tablewrap" tabindex="0" role="region" aria-label="{escape(caption)}">'
            f'<table><caption class="data-caption">{escape(caption)}</caption><thead><tr>{columns}'
            f'</tr></thead><tbody>{"".join(rows)}</tbody></table></div>')


def metrics(label, r):
    return [label, f'{r["n"]:,}', f'${r["baseline"]:,.2f}', f'${r["after"]:,.2f}',
            f'{r["tokens"]:.1f}%', f'{r["savings"]:.1f}%']


def aggregate(run):
    return table("Published results by strategy", COLUMNS,
                 [metrics(ARM[k], run["arms"][k][-1]) for k in sorted(run["arms"])])


def figure(asset, caption):
    return f'<figure class="plot"><img src="{{{{root}}}}assets/{asset}.svg" alt="{caption}"><figcaption>{caption}</figcaption></figure>'


def replay(slug):
    run = data(slug)
    body = "{{head}}" + aggregate(run)
    body += f'<p class="fine">Run date {run["date"]} · {run["sessions"]} SWE-chat sessions · Company-published · Model costs before service fees</p>'
    body += f'<h2>Reading this replay</h2><p>{RUNS[slug][2]}</p><h2>How savings change with context length</h2>'
    body += '<p>Turns are grouped by the length of the original input. Negative savings indicate an increase. The overall percentage is calculated from totals.</p>'
    if slug == "opus-rewrite":
        body += figure("cost-by-context", "Condense model cost savings by original input length. Exact figures follow below.")
    for arm, values in sorted(run["arms"].items()):
        body += table(ARM[arm], ["Input length", *COLUMNS[1:]], [metrics(r["bucket"], r) for r in values if r["bucket"] != "ALL"])
    body += f'<h2>Inspect the evidence</h2><p><a href="{BENCH}/tree/{PIN}/runs/run-{run["uuid"]}">Archived run files</a> · <a href="{{{{root}}}}data/{slug}.json">Download the chart data</a> · <a href="{BENCH}/blob/main/docs/cost.md">Replay methodology</a></p>'
    return body + '<p><a href="{{root}}compare/headroom/">Condense vs. Headroom</a> · <a href="{{root}}benchmarks/quality/">Task-quality benchmarks</a></p>'


def catalog():
    body = '{{head}}<h2>Cost replays and task completion</h2><ul class="rows">'
    for slug, (title, description, _) in RUNS.items():
        body += f'<li><a href="{{{{root}}}}benchmarks/{slug}/">{title}</a><p>{description}</p></li>'
    body += '<li><a href="{{root}}benchmarks/quality/">Does the agent finish the task?</a><p>Verifier results on long Terminal-Bench tasks, next-step scoring and a separate trial where compression increased cost.</p></li></ul><p><a href="{{root}}compare/headroom/">Compare Condense and Headroom on identical turns →</a></p>'
    body += '<h2>Compressor evaluations and field results</h2><p>These studies measure different things, from question-answering accuracy to the cost of a production agent. Each entry links to its source.</p><ul class="rows">'
    for item in data("studies"):
        body += '<li><div><small>{classification}</small><br><a href="{url}">{title} ↗</a></div><div><p>{result}</p><p class="fine">{caveat}</p></div></li>'.format(**{k: escape(v) for k,v in item.items()})
    return body + '</ul>'


def build(output, base, preview):
    pages = {
        "": ("Context compression for coding agents", "Condense dense CLI | Context compression for coding agents", "Install dense for Claude Code, Codex or OpenCode and explore published benchmarks for model cost, input tokens and verified task completion.", template("home")),
        "benchmarks/": ("Context compression benchmarks", "LLM context compression benchmarks | Condense", "Explore coding-agent replays, task completion, compressor evaluations and production results. Each study includes the workload, measured outcome and source.", catalog()),
        "compare/headroom/": ("Condense vs. Headroom", "Condense vs Headroom | Coding-agent cost and quality benchmarks", "On identical Claude Haiku 4.5 turns, Condense saved 27.5% of model cost, Headroom cache mode 13.8% and Kompress 1.8%. Explore the cost and quality evidence.", template("comparison")),
        "benchmarks/quality/": ("Does the agent still finish the task?", "Coding-agent task completion under context compression | Condense", "A cost replay holds the conversation fixed. These tests let the agent choose its next action or run a full Terminal-Bench task against a verifier.", template("quality"))
    }
    for slug, (heading, desc, _) in RUNS.items():
        pages[f"benchmarks/{slug}/"] = (heading, heading+" | Condense benchmarks", desc, replay(slug))
    common = table("All three strategies on identical turns", COLUMNS, [
        ["Condense","1,571","$17.63","$12.79","50.5%","27.5%"], ["Headroom · cache mode","1,571","$17.63","$15.20","15.2%","13.8%"],
        ["Headroom · Kompress","1,571","$17.63","$17.31","11.8%","1.8%"]])
    verifier = table("Full-task verifier · Opus 4.8", ["Strategy","Successful trials","Solve rate","95% interval"], [
        ["Vanilla Claude Code","23 / 34","68%","51–81%"], ["Condense","11 / 14","79%","52–92%"], ["Headroom","10 / 14","71%","45–88%"]])
    steps = table("192 paired steps", ["Metric","Control","Condense"], [
        ["Exact action match","46%","24%"], ["Action judged useful","58%","63%"], ["Average context","260k tokens","99k tokens"], ["Model cost","$33.61","$18.53"]])
    output.mkdir(parents=True, exist_ok=True)
    for folder in ("data", "assets"):
        source_folder = SOURCE.parent / "docs" / folder
        if source_folder != output/folder:
            shutil.copytree(source_folder, output/folder, dirs_exist_ok=True)
    for route, (heading,title,desc,body) in pages.items():
        root = "../"*route.count("/") or "./"
        url = base + route
        crumbs = [("dense","")]
        if route.startswith("benchmarks/") and route != "benchmarks/":
            crumbs.append(("Benchmarks","benchmarks/"))
        if route:
            crumbs.append((heading,route))
        trail = " / ".join(f'<a href="{root}{path}">{escape(label)}</a>' for label,path in crumbs[:-1])
        lead = desc if route != "compare/headroom/" else "How two context-compression proxies changed the model bill on the same coding sessions, and what happened when agents ran full tasks."
        head = f'<header class="pagehead"><nav class="crumb" aria-label="Breadcrumb">{trail} / {escape(heading)}</nav><h1>{escape(heading)}</h1><p class="lede">{escape(lead)}</p></header>'
        body = fill(body, dict(root=root,head=head,common_table=common,capped_table=aggregate(data("haiku-capped")),verifier=verifier,
                               action_plot=figure("action-vs-goal","Agreement with the recorded action and the share judged useful on the same trace."),
                               replay_plot=figure("replay-by-step","Cumulative input-token and model cost savings at the recorded checkpoints."),step_table=steps)).replace("{{root}}",root)
        if route:
            body = f'<article class="wrap story body-end">{body}</article>'
        graph = [{"@type":"WebPage","@id":url,"name":title,"description":desc,"publisher":{"@type":"Organization","name":"Condense","url":"https://condense.chat/"}}]
        if route:
            graph.append({"@type":"BreadcrumbList","itemListElement":[{"@type":"ListItem","position":i,"name":label,"item":base+path} for i,(label,path) in enumerate(crumbs,1)]})
        else:
            graph.append({"@type":"SoftwareSourceCode","name":"dense","codeRepository":"https://github.com/condense-chat/dense","url":url})
        nav = ""
        for label, path in [("Benchmarks","benchmarks/"),("vs. Headroom","compare/headroom/")]:
            current = ' aria-current="page"' if route == path else ''
            nav += f'<a href="{root}{path}"{current}>{label}</a>'
        html = fill(template("layout"), dict(title=escape(title),description=escape(desc),url=escape(url),base=escape(base),root=root,
                    schema=json.dumps({"@context":"https://schema.org","@graph":graph},ensure_ascii=False),nav=nav,body=body,
                    robots='<meta name="robots" content="noindex, nofollow">' if preview else ''))
        target = output/route/"index.html"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(html)
    xml = ET.Element("urlset", xmlns="http://www.sitemaps.org/schemas/sitemap/0.9")
    for route in pages:
        ET.SubElement(ET.SubElement(xml,"url"),"loc").text = base + route
    ET.ElementTree(xml).write(output/"sitemap.xml",encoding="utf-8",xml_declaration=True)
    (output/".nojekyll").touch()
    print(f"Built {len(pages)} pages at {output}")


if __name__ == "__main__":
    parser = ArgumentParser()
    parser.add_argument("output",type=Path)
    parser.add_argument("--base-url",default="https://condense-chat.github.io/dense/")
    parser.add_argument("--preview",action="store_true")
    args = parser.parse_args()
    url = urlsplit(args.base_url)
    if url.scheme != "https" or not url.netloc or url.query or url.fragment:
        parser.error("Use an absolute HTTPS deployment URL.")
    build(args.output.resolve(), args.base_url.rstrip("/")+"/", args.preview)
