#!/usr/bin/env python3
"""YJLCoder 全球远程岗位猎手。

脚本只依赖 Python 标准库。模型负责决定何时运行；抓取、字段归一化、
去重、换汇、时区换算、评分和统计均由这里确定性完成。
"""

from __future__ import annotations

import argparse
import concurrent.futures
import csv
import datetime as dt
import email.utils
import hashlib
import html
from html.parser import HTMLParser
import http.client
import json
import math
import os
from pathlib import Path
import re
import ssl
import subprocess
import sys
import threading
import time
from typing import Any, Iterable
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from zoneinfo import ZoneInfo


ROOT = Path(__file__).resolve().parent.parent
REFERENCES = ROOT / "references"
USER_AGENT = "YJLCoder-RemoteJobHunter/1.1 (+https://github.com/ksk2kk/YujialeCode)"
DEFAULT_TIMEOUT = 15
BEIJING = ZoneInfo("Asia/Shanghai")
UTC = dt.timezone.utc
QUERY_MATRIX = [
    "Golang blockchain backend",
    "Go distributed systems remote",
    "Rust protocol engineer",
    "Rust Solana remote",
    "C++ systems remote",
    "Web3 infrastructure engineer",
    "Blockchain node RPC indexer",
    "Cosmos SDK Golang",
    "Substrate Rust",
    "Remote worldwide",
    "Remote APAC",
]
REMOTE_WORDS = ("remote", "worldwide", "work from home", "distributed team", "anywhere")
WORLDWIDE_WORDS = ("worldwide", "anywhere", "global remote", "work from anywhere")
COUNTRY_BLOCK_WORDS = (
    "us only", "u.s. only", "united states only", "must be based in the us", "us-based only",
    "canada only", "uk only", "europe only", "eu only", "remote-us", "remote us",
)
PERIOD_FACTORS = {
    "year": 1.0, "annual": 1.0, "annum": 1.0, "yr": 1.0,
    "month": 12.0, "monthly": 12.0,
    "week": 52.0, "weekly": 52.0,
    "day": 260.0, "daily": 260.0,
    "hour": 2080.0, "hourly": 2080.0, "hr": 2080.0,
}
CURRENCY_SYMBOLS = {"$": "USD", "€": "EUR", "£": "GBP", "¥": "CNY", "₹": "INR"}
SALARY_CONTEXT = re.compile(
    r"(?i)salary|compensation|base\s+pay|pay\s+range|annual\s+pay|remuneration|"
    r"wages?|hourly\s+rate|day\s+rate|starting\s+at|earnings?"
)
NON_SALARY_MONEY_CONTEXT = re.compile(
    r"(?i)fund(?:ing|ed|raise)|valuation|revenue|assets?\s+under\s+management|\bAUM\b|"
    r"market\s+cap|trading\s+volume|transactions?|investment|series\s+[a-f]|bounty|prize"
)
BENEFIT_MONEY_CONTEXT = re.compile(
    r"(?i)(?:learning|development|training|education|equipment|wellness|home.office).{0,45}budget|"
    r"budget.{0,45}(?:learning|development|training|education|equipment|wellness|home.office)|"
    r"reimbursement|expense\s+budget|benefit\s+allowance"
)
TZ_ALIASES = {
    "utc": "UTC", "gmt": "UTC", "bst": "Europe/London",
    "cet": "Europe/Paris", "cest": "Europe/Paris", "europe": "Europe/Paris",
    "est": "America/New_York", "edt": "America/New_York", "et": "America/New_York",
    "cst": "America/Chicago", "cdt": "America/Chicago", "ct": "America/Chicago",
    "mst": "America/Denver", "mdt": "America/Denver", "mt": "America/Denver",
    "pst": "America/Los_Angeles", "pdt": "America/Los_Angeles", "pt": "America/Los_Angeles",
    "apac": "Asia/Singapore", "asia": "Asia/Singapore", "aest": "Australia/Sydney",
}


def load_json(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)


def now_iso() -> str:
    return dt.datetime.now(UTC).replace(microsecond=0).isoformat()


def as_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, (list, tuple, set)):
        return "; ".join(as_text(v) for v in value if v is not None)
    if isinstance(value, dict):
        return "; ".join(f"{k}: {as_text(v)}" for k, v in value.items())
    return str(value)


def clean_html(value: Any) -> str:
    text = as_text(value)
    text = re.sub(r"(?is)<(script|style).*?>.*?</\1>", " ", text)
    text = re.sub(r"(?s)<[^>]+>", " ", text)
    text = html.unescape(text)
    return re.sub(r"\s+", " ", text).strip()


def parse_date(value: Any) -> str | None:
    if value in (None, "", 0):
        return None
    if isinstance(value, (int, float)):
        stamp = float(value)
        if stamp > 10_000_000_000:
            stamp /= 1000
        try:
            return dt.datetime.fromtimestamp(stamp, UTC).replace(microsecond=0).isoformat()
        except (ValueError, OSError):
            return None
    raw = str(value).strip()
    if not raw:
        return None
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", raw):
        return raw + "T00:00:00+00:00"
    try:
        parsed = dt.datetime.fromisoformat(raw.replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=UTC)
        return parsed.astimezone(UTC).replace(microsecond=0).isoformat()
    except ValueError:
        pass
    try:
        parsed = email.utils.parsedate_to_datetime(raw)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=UTC)
        return parsed.astimezone(UTC).replace(microsecond=0).isoformat()
    except (TypeError, ValueError, OverflowError):
        return None


def age_days(value: str | None) -> float | None:
    if not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=UTC)
        return max(0.0, (dt.datetime.now(UTC) - parsed.astimezone(UTC)).total_seconds() / 86400)
    except ValueError:
        return None


def is_active(job: dict[str, Any], freshness_days: int) -> bool:
    expires = job.get("expires_at")
    if expires:
        try:
            when = dt.datetime.fromisoformat(expires.replace("Z", "+00:00"))
            if when.tzinfo is None: when = when.replace(tzinfo=UTC)
            if when.astimezone(UTC) < dt.datetime.now(UTC): return False
        except ValueError:
            pass
    age = age_days(job.get("published_at"))
    return age is None or age <= max(1, freshness_days)


def canonical_url(url: str) -> str:
    try:
        p = urllib.parse.urlsplit(url)
        query = urllib.parse.parse_qsl(p.query, keep_blank_values=True)
        query = [(k, v) for k, v in query if not k.lower().startswith("utm_") and k.lower() not in {"ref", "source", "gh_src"}]
        path = re.sub(r"/+", "/", p.path).rstrip("/") or "/"
        return urllib.parse.urlunsplit((p.scheme.lower(), p.netloc.lower(), path, urllib.parse.urlencode(query), ""))
    except ValueError:
        return url


class HttpClient:
    """有界、限速、可审计缓存的 HTTP 客户端。"""

    def __init__(self, cache_dir: Path, ttl: int = 1800, timeout: int = DEFAULT_TIMEOUT):
        self.cache_dir = cache_dir
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.ttl = ttl
        self.timeout = timeout
        self.host_last: dict[str, float] = {}
        self.context = ssl.create_default_context()
        self.thread_state = threading.local()

    def set_deadline(self, deadline: float) -> None:
        self.thread_state.deadline = deadline

    def clear_deadline(self) -> None:
        self.thread_state.deadline = None

    def remaining(self) -> float | None:
        deadline = getattr(self.thread_state, "deadline", None)
        return None if deadline is None else deadline - time.monotonic()

    def get(self, url: str, headers: dict[str, str] | None = None, force: bool = False) -> bytes:
        key = hashlib.sha256((url + json.dumps(headers or {}, sort_keys=True)).encode()).hexdigest()
        body_path = self.cache_dir / f"{key}.body"
        meta_path = self.cache_dir / f"{key}.json"
        if not force and body_path.exists() and meta_path.exists():
            try:
                meta = load_json(meta_path)
                if time.time() - float(meta["saved_at"]) <= self.ttl:
                    return body_path.read_bytes()
            except (OSError, KeyError, ValueError, json.JSONDecodeError):
                pass
        host = urllib.parse.urlsplit(url).netloc.lower()
        wait = 0.8 - (time.monotonic() - self.host_last.get(host, 0.0))
        if wait > 0:
            time.sleep(wait)
        req_headers = {"User-Agent": USER_AGENT, "Accept": "application/json, application/rss+xml, application/xml, text/html;q=0.9, */*;q=0.5"}
        req_headers.update(headers or {})
        request = urllib.request.Request(url, headers=req_headers)
        error: Exception | None = None
        for attempt in range(2):
            try:
                remaining = self.remaining()
                if remaining is not None and remaining <= 0:
                    raise TimeoutError("该来源超过抓取时间预算")
                self.host_last[host] = time.monotonic()
                request_timeout = self.timeout if remaining is None else max(1.0, min(self.timeout, remaining))
                with urllib.request.urlopen(request, timeout=request_timeout, context=self.context) as response:
                    partial = False
                    try:
                        data = response.read(12_000_000)
                    except http.client.IncompleteRead as exc:
                        # 部分招聘站错误声明 Content-Length，但已收到的 HTML 仍完整
                        # 包含岗位卡片/JSON-LD。小于 1 KiB 的碎片不采用，也不缓存。
                        data = exc.partial
                        partial = True
                    if not data:
                        raise RuntimeError("空响应")
                    if partial and len(data) < 1024:
                        raise RuntimeError("响应传输不完整且可用正文过少")
                    if not partial:
                        body_path.write_bytes(data)
                        atomic_write(meta_path, json.dumps({"url": url, "saved_at": time.time(), "status": response.status}, ensure_ascii=False))
                    return data
            except urllib.error.HTTPError as exc:
                error = exc
                if exc.code not in {429, 500, 502, 503, 504}:
                    break
                retry = exc.headers.get("Retry-After")
                time.sleep(min(8.0, float(retry) if retry and retry.isdigit() else 1.5 * (attempt + 1)))
            except (urllib.error.URLError, TimeoutError, OSError) as exc:
                error = exc
                time.sleep(1.0 * (attempt + 1))
        if body_path.exists():
            return body_path.read_bytes()
        raise RuntimeError(f"请求失败 {url}: {error}")

    def json(self, url: str, headers: dict[str, str] | None = None) -> Any:
        return json.loads(self.get(url, headers).decode("utf-8", errors="replace"))


class PageParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[tuple[str, str]] = []
        self.jsonld: list[str] = []
        self.meta: dict[str, str] = {}
        self.h1: list[str] = []
        self.title: list[str] = []
        self.text: list[str] = []
        self._anchor: str | None = None
        self._anchor_text: list[str] = []
        self._jsonld = False
        self._json_buf: list[str] = []
        self._capture: str | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        a = {k.lower(): (v or "") for k, v in attrs}
        if tag == "a" and a.get("href"):
            self._anchor, self._anchor_text = a["href"], []
        elif tag == "script" and "ld+json" in a.get("type", "").lower():
            self._jsonld, self._json_buf = True, []
        elif tag == "meta":
            key = (a.get("property") or a.get("name") or "").lower()
            if key and a.get("content"):
                self.meta[key] = a["content"]
        elif tag in {"h1", "title"}:
            self._capture = tag

    def handle_endtag(self, tag: str) -> None:
        if tag == "a" and self._anchor is not None:
            self.links.append((self._anchor, clean_html(" ".join(self._anchor_text))))
            self._anchor, self._anchor_text = None, []
        elif tag == "script" and self._jsonld:
            self.jsonld.append("".join(self._json_buf))
            self._jsonld, self._json_buf = False, []
        elif tag == self._capture:
            self._capture = None

    def handle_data(self, data: str) -> None:
        if self._jsonld:
            self._json_buf.append(data)
            return
        if data.strip():
            self.text.append(data.strip())
            if self._anchor is not None:
                self._anchor_text.append(data)
            if self._capture == "h1":
                self.h1.append(data.strip())
            elif self._capture == "title":
                self.title.append(data.strip())


def walk_json(value: Any) -> Iterable[dict[str, Any]]:
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from walk_json(child)
    elif isinstance(value, list):
        for child in value:
            yield from walk_json(child)


def blank_job(source: dict[str, Any], url: str = "") -> dict[str, Any]:
    return {
        "source_id": source["id"], "source": source["name"], "source_priority": source.get("priority", 50),
        "source_url": source.get("homepage", ""), "job_id": "", "title": "", "company": "未公开",
        "description": "", "url": canonical_url(url), "apply_url": "", "location": "未公开",
        "remote": False, "remote_scope": "unknown", "timezone_original": "未公开",
        "beijing_hours": "未公开", "schedule_confidence": "unknown", "employment_type": "未公开",
        "contract_duration": "未公开", "weekly_hours": None, "experience_min_years": None,
        "experience_max_years": None, "salary_original": "未公开", "salary_min": None,
        "salary_max": None, "salary_currency": None, "salary_period": None,
        "salary_usd_annual_min": None, "salary_usd_annual_max": None,
        "salary_cny_annual_min": None, "salary_cny_annual_max": None, "salary_kind": "unknown",
        "salary_confidence": "unknown", "salary_evidence": "", "salary_rejected_reason": "",
        "source_salary_trust": source.get("salary_trust", "direct"),
        "source_eligibility_trust": source.get("eligibility_trust", "direct"),
        "published_at": None, "expires_at": None, "fetched_at": now_iso(), "skills": [],
        "missing_skills": [], "eligibility": "unknown", "eligibility_reason": "未发现明确全球可申请说明",
        "difficulty": None, "difficulty_label": "未知", "score": 0.0, "score_breakdown": {},
        "confidence": "medium", "warnings": [],
    }


def organization_name(value: Any) -> str:
    if isinstance(value, dict):
        return as_text(value.get("name") or value.get("legalName")) or "未公开"
    return as_text(value) or "未公开"


def location_text(value: Any) -> str:
    parts: list[str] = []
    for obj in walk_json(value):
        if obj.get("@type") in {"Country", "Place"} and obj.get("name"):
            parts.append(as_text(obj.get("name")))
        if obj.get("@type") == "PostalAddress" or any(k in obj for k in ("addressLocality", "addressRegion", "addressCountry")):
            parts.extend(as_text(obj.get(k)) for k in ("addressLocality", "addressRegion", "addressCountry") if obj.get(k))
    if parts:
        return ", ".join(dict.fromkeys(parts))
    if isinstance(value, (dict, list)):
        return "未公开"
    return clean_html(value) or "未公开"


def job_from_jsonld(obj: dict[str, Any], source: dict[str, Any], page_url: str) -> dict[str, Any]:
    job = blank_job(source, as_text(obj.get("url")) or page_url)
    job.update({
        "job_id": as_text(obj.get("identifier") or obj.get("@id")),
        "title": clean_html(obj.get("title") or obj.get("name")),
        "company": organization_name(obj.get("hiringOrganization")),
        "description": clean_html(obj.get("description"))[:120000],
        "location": location_text(obj.get("jobLocation") or obj.get("applicantLocationRequirements")),
        "employment_type": clean_html(obj.get("employmentType")) or "未公开",
        "contract_duration": clean_html(obj.get("jobBenefits") if "contract" in clean_html(obj.get("employmentType")).lower() else "") or "未公开",
        "published_at": parse_date(obj.get("datePosted")),
        "expires_at": parse_date(obj.get("validThrough")),
    })
    remote_type = as_text(obj.get("jobLocationType")).upper()
    remote_hint = " ".join((job["location"], job["description"][:3000], remote_type)).lower()
    job["remote"] = "TELECOMMUTE" in remote_type or any(x in remote_hint for x in REMOTE_WORDS)
    salary_key = "baseSalary" if obj.get("baseSalary") else ("estimatedSalary" if obj.get("estimatedSalary") else "")
    salary = obj.get(salary_key) if salary_key else None
    if salary:
        for node in walk_json(salary):
            if any(k in node for k in ("minValue", "maxValue", "value")):
                val = node.get("value")
                if isinstance(val, dict):
                    val_node = val
                else:
                    val_node = node
                job["salary_min"] = number(val_node.get("minValue", val_node.get("value")))
                job["salary_max"] = number(val_node.get("maxValue", val_node.get("value")))
                job["salary_currency"] = as_text(node.get("currency") or obj.get("salaryCurrency")).upper() or None
                job["salary_period"] = as_text(val_node.get("unitText")).lower() or None
                break
        job["salary_original"] = clean_html(salary)
        job["salary_kind"] = "estimated" if salary_key == "estimatedSalary" else "explicit_structured"
        job["salary_confidence"] = "low" if salary_key == "estimatedSalary" else "high"
        job["salary_evidence"] = f"JobPosting.{salary_key}: {job['salary_original'][:500]}"
    return job


def number(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if value is None:
        return None
    raw = str(value).replace("\u00a0", " ").strip().lower()
    raw = re.sub(r"(?<=\d)[,\s](?=\d{3}(?:\D|$))", "", raw)
    match = re.search(r"-?\d+(?:\.\d+)?", raw)
    if not match:
        return None
    n = float(match.group())
    if "k" in raw[match.end():match.end() + 2]:
        n *= 1000
    elif "m" in raw[match.end():match.end() + 2]:
        n *= 1_000_000
    return n


def parse_salary_text(text: str, assume_salary_field: bool = False) -> dict[str, Any]:
    """只从有薪资语境的局部文本中提取金额。

    招聘正文里常同时出现融资额、交易量和客户资产。旧实现会选最大的货币数字，
    等于在超市小票里把“门店年营收”当成收银员工资。本实现按语境可信度选值，
    并保留命中片段供人工审计。
    """
    compact = text.replace("\u00a0", " ")
    compact = re.sub(r"(?<=\d)[,\s](?=\d{3}(?:\D|$))", "", compact)
    pattern = re.compile(
        r"(?i)(?P<currency>USD|EUR|GBP|CAD|AUD|CNY|RMB|CHF|SEK|NOK|DKK|PLN|INR|\$|€|£|¥)?\s*"
        r"(?P<low>\d+(?:\.\d+)?\s*[km]?)\s*(?P<sep>-|–|—|to)?\s*"
        r"(?:USD|EUR|GBP|CAD|AUD|CNY|RMB|CHF|SEK|NOK|DKK|PLN|INR|\$|€|£|¥)?\s*"
        r"(?P<high>\d+(?:\.\d+)?\s*[km]?)?\s*"
        r"(?P<currency2>USD|EUR|GBP|CAD|AUD|CNY|RMB|CHF|SEK|NOK|DKK|PLN|INR)?\s*"
        r"(?:/|per\s+|a\s+)?(?P<period>year|annual|annum|yr|month|monthly|week|weekly|day|daily|hour|hourly|hr)?"
    )
    candidates: list[tuple[int, int, str, float, float, str, str | None, str]] = []
    for m in pattern.finditer(compact):
        currency_raw = m.group("currency") or m.group("currency2")
        period = (m.group("period") or "").lower()
        low = number(m.group("low"))
        high = number(m.group("high")) if m.group("high") else low
        # 年限、工时、版本号里也经常出现 “5 years/20 hour”。没有明确货币时
        # 宁可留空，也不能把它们包装成薪资。
        if not currency_raw:
            continue
        context = compact[max(0, m.start() - 120):m.end() + 120]
        local_money_context = compact[max(0, m.start() - 70):m.end() + 70]
        salary_context = bool(SALARY_CONTEXT.search(context))
        # 已知 salary 字段仍要做语义排雷；某些聚合 API 曾把融资简介塞进 salary。
        if NON_SALARY_MONEY_CONTEXT.search(context) and not salary_context:
            continue
        if BENEFIT_MONEY_CONTEXT.search(local_money_context):
            continue
        short_salary_field = assume_salary_field and len(compact) <= 240
        explicit_rate = bool(period and currency_raw)
        if not salary_context and not short_salary_field and not explicit_rate:
            continue
        currency = CURRENCY_SYMBOLS.get(currency_raw, currency_raw.upper() if currency_raw else None)
        if currency == "RMB":
            currency = "CNY"
        if low is not None and low >= 1:
            score = (5 if salary_context else 2) + (2 if period else 0) + (1 if m.group("sep") and m.group("high") else 0)
            evidence = re.sub(r"\s+", " ", context).strip()[:360]
            candidates.append((score, -m.start(), m.group(0).strip(), low, high, currency, period or None, evidence))
    if not candidates:
        return {}
    chosen = max(candidates, key=lambda x: (x[0], x[1]))
    return {"salary_original": chosen[2], "salary_min": chosen[3], "salary_max": chosen[4],
            "salary_currency": chosen[5], "salary_period": chosen[6], "salary_kind": "explicit_text",
            "salary_confidence": "high" if SALARY_CONTEXT.search(chosen[7]) else "medium",
            "salary_evidence": chosen[7]}


def parse_experience(text: str) -> tuple[float | None, float | None]:
    values: list[tuple[float, float]] = []
    for match in re.finditer(r"(?i)(\d+(?:\.\d+)?)\s*(?:-|–|to)?\s*(\d+(?:\.\d+)?)?\s*\+?\s*(?:years?|yrs?)", text):
        before = text[max(0, match.start() - 80):match.start()].lower()
        after = text[match.end():match.end() + 80].lower()
        qualified = (
            re.search(r"experience|required|requirement|minimum|at least|professional|hands.on", before)
            or re.match(r"\s*(?:of\s+[^.;]{0,50}\s+experience|experience)", after)
        )
        if not qualified:
            continue
        low = float(match.group(1))
        high = float(match.group(2) or low)
        if 0 <= low <= 30 and low <= high <= 40:
            values.append((low, high))
    if not values:
        return None, None
    return max(values, key=lambda pair: pair[0])


def parse_hours(text: str) -> tuple[float | None, str]:
    weekly = re.search(r"(?i)(\d{1,2})(?:\s*(?:-|–|to)\s*(\d{1,2}))?\s*(?:hours?|hrs?)\s*(?:per|a)?\s*week", text)
    if weekly:
        low, high = float(weekly.group(1)), float(weekly.group(2) or weekly.group(1))
        return (low + high) / 2, weekly.group(0)
    duration = re.search(r"(?i)(\d+)\s*(?:months?|weeks?)\s*(?:contract|engagement|project)", text)
    return None, duration.group(0) if duration else "未公开"


def infer_remote(job: dict[str, Any]) -> None:
    location_only = job.get("location", "").lower().strip()
    location_title = " ".join((location_only, job.get("title", ""))).lower()
    description = job.get("description", "")[:20000].lower()
    explicit_remote = re.search(
        r"(?:this|the)\s+(?:role|position|job)\s+is\s+(?:fully\s+)?remote|"
        r"(?:fully|100%)\s+remote|work\s+remotely|remote\s+(?:role|position|job|opportunity)|"
        r"work\s+from\s+(?:home|anywhere)",
        description,
    )
    explicit_onsite = re.search(
        r"this\s+(?:role|position|job)\s+will\s+be\s+in\s+(?:the\s+)?office|"
        r"(?:on.?site|in.office)\s+(?:role|position|job|\d\s+days)|not\s+(?:a\s+)?remote\s+(?:role|position|job)|"
        r"must\s+work\s+(?:on.?site|in\s+the\s+office)",
        description,
    )
    job["remote"] = False if explicit_onsite else (bool(job.get("remote")) or any(word in location_title for word in REMOTE_WORDS) or bool(explicit_remote))
    scope_text = location_title + " " + description[:6000]
    worldwide_remote = re.search(
        r"remote.{0,50}(?:worldwide|anywhere(?:\s+in\s+the\s+world)?)|"
        r"(?:worldwide|anywhere(?:\s+in\s+the\s+world)?).{0,50}remote|work\s+from\s+anywhere",
        scope_text,
    )
    if location_only in {"anywhere", "worldwide", "global", "remote - anywhere", "remote anywhere", "remote - worldwide", "remote worldwide"}:
        job["remote_scope"] = "worldwide"
    elif worldwide_remote:
        job["remote_scope"] = "worldwide"
    elif re.search(r"\b(apac|asia|asian time zones?)\b", location_title):
        job["remote_scope"] = "APAC"
    elif re.search(r"\b(europe|emea|eu only|cet|cest)\b", location_title):
        job["remote_scope"] = "Europe/EMEA"
    elif re.search(r"\b(united states|usa|us|us only|u\.s\.|remote-us|canada|americas?)\b", location_title):
        job["remote_scope"] = "Americas/North America"
    elif job["remote"]:
        job["remote_scope"] = "remote-unspecified"


def infer_schedule(job: dict[str, Any]) -> None:
    text = " ".join((job.get("location", ""), job.get("description", "")[:15000])).lower()
    tz_name = None
    label = None
    for alias, zone in TZ_ALIASES.items():
        if re.search(rf"(?<![a-z]){re.escape(alias)}(?![a-z])", text):
            tz_name, label = zone, alias.upper()
            break
    offset = re.search(r"(?i)UTC\s*([+-])\s*(\d{1,2})(?::?(\d{2}))?", text)
    if offset:
        minutes = (int(offset.group(2)) * 60 + int(offset.group(3) or 0)) * (1 if offset.group(1) == "+" else -1)
        zone = dt.timezone(dt.timedelta(minutes=minutes))
        label = f"UTC{offset.group(1)}{offset.group(2)}:{offset.group(3) or '00'}"
    elif tz_name:
        zone = ZoneInfo(tz_name)
    else:
        job["timezone_original"] = "未公开"
        job["beijing_hours"] = "未公开"
        return
    today = dt.datetime.now(BEIJING).date()
    start = dt.datetime.combine(today, dt.time(9), zone).astimezone(BEIJING)
    end = dt.datetime.combine(today, dt.time(17), zone).astimezone(BEIJING)
    def clock(x: dt.datetime) -> str:
        delta = (x.date() - today).days
        suffix = "" if delta == 0 else ("次日" if delta == 1 else "前一日")
        return f"{suffix}{x:%H:%M}"
    job["timezone_original"] = label or tz_name
    job["beijing_hours"] = f"约 {clock(start)}–{clock(end)}（按当地 09:00–17:00 推算）"
    job["schedule_confidence"] = "inferred"


def default_rates() -> dict[str, float]:
    """每 1 单位货币折算 USD。仅作为离线估算并在报告标注。"""
    return {"USD": 1.0, "EUR": 1.17, "GBP": 1.35, "CAD": 0.73, "AUD": 0.65, "CNY": 0.139,
            "CHF": 1.25, "SEK": 0.106, "NOK": 0.099, "DKK": 0.157, "PLN": 0.276, "INR": 0.0114}


def fetch_rates(client: HttpClient) -> tuple[dict[str, float], str]:
    rates = default_rates()
    try:
        raw = client.get("https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml")
        root = ET.fromstring(raw)
        eur_per: dict[str, float] = {"EUR": 1.0}
        for node in root.iter():
            if node.attrib.get("currency") and node.attrib.get("rate"):
                eur_per[node.attrib["currency"]] = float(node.attrib["rate"])
        if "USD" in eur_per:
            usd_per_eur = eur_per["USD"]
            rates = {cur: usd_per_eur / units for cur, units in eur_per.items()}
            rates["USD"] = 1.0
            return rates, "ECB daily reference rates"
    except Exception:
        pass
    return rates, "offline fallback rates (estimate)"


def apply_source_provenance_policy(job: dict[str, Any]) -> None:
    """把聚合站的推测字段与招聘正文的明确信息分开。

    聚合站可以帮助发现岗位，却不能代替雇主承诺薪资或可雇佣国家。来源策略
    跟着每条记录传递，避免在后续去重和重新分析时丢失这层边界。
    """
    if job.get("salary_kind") == "explicit":
        job["salary_kind"] = "explicit_structured" if "MonetaryAmount" in job.get("salary_original", "") else "explicit_text"
        if job.get("salary_confidence") in (None, "", "unknown"):
            job["salary_confidence"] = "medium"
    if job.get("source_salary_trust") == "estimate" and job.get("salary_min") is not None:
        estimated = {
            "original": job.get("salary_original"), "min": job.get("salary_min"),
            "max": job.get("salary_max"), "currency": job.get("salary_currency"),
            "period": job.get("salary_period"),
        }
        explicit = parse_salary_text(job.get("description", ""), assume_salary_field=False)
        if explicit:
            job.update(explicit)
            job["warnings"].append("聚合站结构化薪资仅作估算；已优先采用招聘正文中的明确薪资")
            job["salary_estimate_original"] = estimated["original"]
        else:
            job["salary_kind"] = "estimated"
            job["salary_confidence"] = "low"
            job["salary_evidence"] = "聚合站结构化字段，原招聘正文未证实"
            job["warnings"].append("薪资来自聚合站估算，不参与收入加分")

    # 聚合站把 Location=Worldwide 当作“可从任何国家受雇”的情况并不可靠。
    if job.get("source_eligibility_trust") == "unverified_aggregator":
        location = re.search(
            r"(?i)(?:^|\b)(?:job\s+)?location\s*[:：]\s*([^.;|]{2,100})",
            job.get("description", "")[:4000],
        )
        if location:
            stated = location.group(1).strip()
            if stated.lower() not in {"remote", "worldwide", "anywhere", "global"}:
                job["location"] = stated
                job["warnings"].append(f"聚合站地点标签未采用；正文写明 Location: {stated}")


def reject_salary(job: dict[str, Any], reason: str) -> None:
    job["salary_rejected_reason"] = reason
    job["warnings"].append(f"薪资字段已隔离：{reason}")
    job["salary_min"] = None
    job["salary_max"] = None
    job["salary_currency"] = None
    job["salary_period"] = None
    job["salary_usd_annual_min"] = None
    job["salary_usd_annual_max"] = None
    job["salary_cny_annual_min"] = None
    job["salary_cny_annual_max"] = None
    job["salary_kind"] = "unknown"
    job["salary_confidence"] = "rejected"


def annualize_salary(job: dict[str, Any], rates: dict[str, float]) -> None:
    if job.get("salary_min") is None:
        salary_field = job.get("salary_original", "")
        parsed = parse_salary_text(salary_field, assume_salary_field=salary_field not in ("", "未公开"))
        if not parsed:
            parsed = parse_salary_text(job.get("description", "")[:20000])
        for key, value in parsed.items():
            if job.get(key) in (None, "", "未公开", "unknown"):
                job[key] = value
    low, high = job.get("salary_min"), job.get("salary_max")
    cur = (job.get("salary_currency") or "").upper()
    if low is None or cur not in rates:
        return
    if BENEFIT_MONEY_CONTEXT.search(job.get("salary_evidence", "")):
        reject_salary(job, "命中学习/培训/设备等福利预算，不是基本薪资")
        return
    if job.get("salary_kind") == "unknown":
        job["salary_kind"] = "explicit_structured"
        job["salary_confidence"] = "high"
        job["salary_evidence"] = job.get("salary_original", "")[:500]
    if high is None:
        high = low
        job["salary_max"] = high
    if low <= 0 or high <= 0 or high < low:
        reject_salary(job, f"原始区间不成立（{low:g}–{high:g} {cur}）")
        return
    period = (job.get("salary_period") or "").lower()
    if not period:
        job["warnings"].append("薪资周期未公开，保留原值但不折算年薪")
        return
    factor = next((v for k, v in PERIOD_FACTORS.items() if k in period), 1.0)
    usd_low = float(low) * factor * rates[cur]
    usd_high = float(high if high is not None else low) * factor * rates[cur]
    # 这里只做明显异常隔离，不把低薪岗位按个人偏好删掉。2 美元年薪或 11M 美元
    # 年薪几乎肯定是字段错位；正常的低收入合同仍会保留。
    if usd_low < 500 or usd_high > 2_000_000 or usd_high / max(usd_low, 1) > 20:
        reject_salary(job, f"折算年薪 {usd_low:,.0f}–{usd_high:,.0f} USD 超出可信边界")
        return
    job["salary_usd_annual_min"] = round(usd_low, 2)
    job["salary_usd_annual_max"] = round(usd_high, 2)
    cny_per_usd = 1 / rates.get("CNY", default_rates()["CNY"])
    job["salary_cny_annual_min"] = round(usd_low * cny_per_usd, 2)
    job["salary_cny_annual_max"] = round(usd_high * cny_per_usd, 2)


def extract_skills(text: str, taxonomy: dict[str, list[str]]) -> list[str]:
    haystack = " " + text.lower() + " "
    found = []
    for skill, aliases in taxonomy.items():
        if any(re.search(rf"(?<![a-z0-9]){re.escape(alias.lower())}(?![a-z0-9])", haystack) for alias in aliases):
            found.append(skill)
    return found


def requirement_relevant_text(description: str) -> str:
    """去掉“其他职位/相关岗位/不是你的技术栈？”等站点尾巴。

    这类尾巴会一次列几十门语言。保留它们等于因为一张招聘页底部有导航菜单，
    就判断平面设计师必须会 Rust、C++、Go；词频和学习路线都会被灌脏。
    """
    markers = (
        "NOT YOUR TECH STACK?", "NOT YOUR STACK?", "OTHER OPEN POSITIONS", "OTHER JOBS",
        "SIMILAR JOBS", "RELATED JOBS", "MORE JOBS AT", "BROWSE MORE JOBS",
    )
    upper = description.upper()
    positions = [upper.find(marker) for marker in markers if upper.find(marker) >= 0]
    return description[:min(positions)] if positions else description


def classify_skill_requirements(text: str, title: str, skills: list[str], taxonomy: dict[str, list[str]]) -> dict[str, str]:
    """粗分必需/加分/仅提及，防止把公司宣传里的技术名当学习任务。"""
    haystack = " " + text.lower() + " "
    title_l = title.lower()
    levels: dict[str, str] = {}
    required_words = re.compile(r"(?i)required?|must|minimum|need(?:ed)?|proficien|strong\s+(?:knowledge|experience)|hands.on|years?\s+of")
    preferred_words = re.compile(r"(?i)preferred|nice\s+to\s+have|bonus|plus|advantage|familiarity")
    for skill in skills:
        contexts = []
        for alias in taxonomy.get(skill, [skill]):
            for match in re.finditer(rf"(?<![a-z0-9]){re.escape(alias.lower())}(?![a-z0-9])", haystack):
                contexts.append(haystack[max(0, match.start() - 100):match.end() + 100])
        if any(alias.lower() in title_l for alias in taxonomy.get(skill, [skill])):
            levels[skill] = "required"
        elif any(preferred_words.search(context) for context in contexts):
            levels[skill] = "preferred"
        elif any(required_words.search(context) for context in contexts):
            levels[skill] = "required"
        else:
            levels[skill] = "mentioned"
    return levels


def normalize_job(job: dict[str, Any], profile: dict[str, Any], taxonomy: dict[str, list[str]], rates: dict[str, float]) -> dict[str, Any]:
    job["title"] = clean_html(job.get("title"))[:500]
    job["company"] = clean_html(job.get("company"))[:300] or "未公开"
    job["description"] = clean_html(job.get("description"))[:120000]
    job["location"] = clean_html(job.get("location"))[:500] or "未公开"
    job["url"] = canonical_url(as_text(job.get("url") or job.get("apply_url")))
    if not job["title"] or not job["url"]:
        raise ValueError("岗位缺少 title 或 url")
    apply_source_provenance_policy(job)
    infer_remote(job)
    requirement_text = requirement_relevant_text(job["description"])
    exp_min, exp_max = parse_experience(requirement_text)
    job["experience_min_years"] = job.get("experience_min_years") if job.get("experience_min_years") is not None else exp_min
    job["experience_max_years"] = job.get("experience_max_years") if job.get("experience_max_years") is not None else exp_max
    hours, duration = parse_hours(job["description"])
    job["weekly_hours"] = job.get("weekly_hours") if job.get("weekly_hours") is not None else hours
    if job.get("contract_duration") in (None, "", "未公开"):
        job["contract_duration"] = duration
    infer_schedule(job)
    job["skills"] = extract_skills(" ".join((job["title"], requirement_text)), taxonomy)
    job["skill_requirements"] = classify_skill_requirements(requirement_text, job["title"], job["skills"], taxonomy)
    profile_skills = set()
    for item in profile.get("languages", []):
        profile_skills.add(item.get("name", ""))
    profile_skills.update(profile.get("other_skills", []))
    job["missing_skills"] = [s for s in job["skills"] if s not in profile_skills]
    annualize_salary(job, rates)
    eligibility(job, profile)
    score_job(job, profile, profile_skills)
    return job


def eligibility(job: dict[str, Any], profile: dict[str, Any]) -> None:
    text = " ".join((job.get("location", ""), job.get("description", "")[:20000])).lower()
    if not job.get("remote"):
        job["eligibility"], job["eligibility_reason"] = "ineligible", "不是远程岗位"
    elif any(word in text for word in COUNTRY_BLOCK_WORDS) or re.search(r"must (?:reside|live|be located) in (?:the )?(?:us|united states|canada|uk|europe|eu)", text):
        job["eligibility"], job["eligibility_reason"] = "ineligible", "职位明确限制候选人所在地，人在中国不符合"
    elif job.get("remote_scope") in {"Americas/North America", "Europe/EMEA"}:
        job["eligibility"], job["eligibility_reason"] = "ineligible", f"远程地域限制为 {job['remote_scope']}，未包含中国"
    elif job.get("source_eligibility_trust") == "unverified_aggregator":
        # 只有原招聘正文的明确措辞才能把聚合站岗位升级为 eligible。卡片上的
        # Worldwide/Remote 只是发现线索，不等于企业能在中国签约或付款。
        direct_scope = re.search(
            r"remote.{0,60}(?:worldwide|anywhere(?:\s+in\s+the\s+world)?|APAC)|"
            r"(?:worldwide|anywhere(?:\s+in\s+the\s+world)?|APAC).{0,60}remote|"
            r"work\s+from\s+anywhere",
            job.get("description", "")[:12000], re.I,
        )
        if direct_scope:
            job["eligibility"], job["eligibility_reason"] = "eligible", "原招聘正文明确写明 Worldwide/APAC remote"
        else:
            job["eligibility"], job["eligibility_reason"] = "unknown", "聚合站远程/Worldwide 标签未得到原招聘正文证实"
            job["warnings"].append("国际资格已降级为待确认：不能用聚合站标签证明可从中国受雇")
    elif job.get("remote_scope") in {"worldwide", "APAC"}:
        job["eligibility"], job["eligibility_reason"] = "eligible", f"公开范围为 {job['remote_scope']}"
    elif re.search(r"(?:candidates?|employees?|applicants?).{0,50}(?:in|from)\s+(?:mainland\s+)?china|(?:work|working)\s+from\s+(?:mainland\s+)?china", text):
        job["eligibility"], job["eligibility_reason"] = "eligible", "岗位明确允许从中国工作"
    else:
        job["eligibility"], job["eligibility_reason"] = "unknown", "远程范围未写清，投递前必须确认可从中国工作"


def score_job(job: dict[str, Any], profile: dict[str, Any], profile_skills: set[str]) -> None:
    skills = set(job.get("skills", []))
    primary = {"Rust", "C++", "Go"}
    skill_score = 0.0
    if skills:
        levels = job.get("skill_requirements", {})
        weights = {"required": 3.0, "preferred": 1.5, "mentioned": .5}
        total_weight = sum(weights.get(levels.get(skill, "mentioned"), .5) for skill in skills)
        matched_weight = sum(weights.get(levels.get(skill, "mentioned"), .5) for skill in skills & profile_skills)
        coverage = 20 * matched_weight / max(.5, total_weight)
        primary_levels = {levels.get(skill, "mentioned") for skill in skills & primary}
        primary_score = 15 if "required" in primary_levels else (9 if "preferred" in primary_levels else (3 if primary_levels else 0))
        skill_score = coverage + primary_score
    elif any(x.lower() in (job["title"] + " " + job["description"]).lower() for x in ("rust", "golang", "c++")):
        skill_score = 25
    eligibility_score = {"eligible": 20, "unknown": 7, "ineligible": 0}[job["eligibility"]]
    salary = job.get("salary_usd_annual_max")
    if salary is None or job.get("salary_kind") not in {"explicit_structured", "explicit_text"}:
        income_score = 6
    else:
        income_score = min(18, 5 + 13 * math.log10(max(1000, salary) / 1000) / 2.5)
    tz = job.get("timezone_original")
    if tz == "未公开":
        timezone_score = 5
    elif any(x in tz for x in ("APAC", "SINGAPORE", "UTC+8", "UTC+08")):
        timezone_score = 12
    elif any(x in tz for x in ("CET", "CEST", "EUROPE", "UTC", "GMT")):
        timezone_score = 8
    else:
        timezone_score = 3 if profile.get("accept_night_shift", True) else 0
    age = age_days(job.get("published_at"))
    freshness = 4 if age is None else max(0, 8 * (1 - age / max(1, profile.get("freshness_days", 45))))
    difficulty = 3.0
    text = (job["title"] + " " + job["description"]).lower()
    if re.search(r"\b(staff|principal|lead|architect|director|head|cto|manager)\b", text): difficulty += 3
    elif re.search(r"\bsenior\b", text): difficulty += 2
    elif re.search(r"\b(junior|entry.level|intern)\b", text): difficulty -= 1.5
    if job.get("experience_min_years") is not None: difficulty += min(2.5, job["experience_min_years"] / 3)
    if re.search(r"ph\.?d|doctorate|security clearance|top secret", text): difficulty += 1.5
    difficulty = max(1, min(10, difficulty))
    job["difficulty"] = round(difficulty, 1)
    job["difficulty_label"] = "低" if difficulty < 3.5 else ("中" if difficulty < 6.5 else "高")
    inverse_difficulty = 7 * (10 - difficulty) / 9
    source_bonus = min(2.0, max(0, (float(job.get("source_priority", 50)) - 50) / 25))
    total = skill_score + eligibility_score + income_score + timezone_score + freshness + inverse_difficulty + source_bonus
    if job["eligibility"] == "ineligible": total = min(total, 25)
    job["score"] = round(min(100, total), 1)
    job["score_breakdown"] = {"skill": round(skill_score, 1), "eligibility": eligibility_score,
                              "income": round(income_score, 1), "timezone": timezone_score,
                              "freshness": round(freshness, 1), "difficulty": round(inverse_difficulty, 1),
                              "source": round(source_bonus, 1),
                              "salary_provenance": job.get("salary_kind", "unknown")}


def parse_jsonld_page(raw: bytes, source: dict[str, Any], page_url: str) -> tuple[list[dict[str, Any]], PageParser]:
    parser = PageParser()
    parser.feed(raw.decode("utf-8", errors="replace"))
    jobs: list[dict[str, Any]] = []
    for block in parser.jsonld:
        try:
            value = json.loads(block.strip())
        except (json.JSONDecodeError, ValueError):
            continue
        for obj in walk_json(value):
            typ = obj.get("@type")
            if typ == "JobPosting" or (isinstance(typ, list) and "JobPosting" in typ):
                jobs.append(job_from_jsonld(obj, source, page_url))
    return jobs, parser


def generic_page_job(raw: bytes, source: dict[str, Any], page_url: str, anchor_title: str = "") -> dict[str, Any] | None:
    jobs, parser = parse_jsonld_page(raw, source, page_url)
    if jobs:
        return jobs[0]
    title = clean_html(" ".join(parser.h1) or parser.meta.get("og:title") or anchor_title)
    all_text = clean_html(" ".join(parser.text))
    if not title or len(all_text) < 80:
        return None
    job = blank_job(source, parser.meta.get("og:url") or page_url)
    job["title"] = re.sub(r"\s+[|–—-]\s+.*$", "", title).strip()
    job["description"] = all_text[:120000]
    job["company"] = clean_html(parser.meta.get("og:site_name")) or "未公开"
    location = re.search(r"(?i)(?:location|remote location)\s*[:：]\s*([^|•\n]{2,120})", all_text)
    job["location"] = location.group(1).strip() if location else "未公开"
    job.update(parse_salary_text(all_text[:30000]))
    job["confidence"] = "low"
    job["warnings"].append("详情页没有 JobPosting JSON-LD，使用容错文本解析")
    return job


def connector_public_html(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    jobs: list[dict[str, Any]] = []
    detail_links: dict[str, str] = {}
    list_errors: list[str] = []
    patterns = [re.compile(p) for p in source.get("job_path_patterns", [r"/jobs?/"])]
    for url in source.get("list_urls", [source["homepage"]]):
        try:
            raw = client.get(url)
        except Exception as exc:
            list_errors.append(f"{url}: {exc}")
            continue
        direct, parser = parse_jsonld_page(raw, source, url)
        jobs.extend(direct)
        base_host = urllib.parse.urlsplit(url).netloc.lower().removeprefix("www.")
        for href, text in parser.links:
            absolute = urllib.parse.urljoin(url, href)
            parts = urllib.parse.urlsplit(absolute)
            host = parts.netloc.lower().removeprefix("www.")
            path_parts = parts.path.strip("/").split("/")
            category_slugs = {"jobs", "remote", "engineering", "marketing", "design", "customer-support", "sales", "operations", "finance", "product", "other", "solana", "cosmos", "rust", "golang", "go"}
            if host != base_host or not any(p.search(parts.path) for p in patterns):
                continue
            if canonical_url(absolute) == canonical_url(url) or len(path_parts) < 2 or path_parts[-1].lower() in category_slugs:
                continue
            detail_links[canonical_url(absolute)] = text
        if len(jobs) >= max_items:
            break
    seen_urls = {j.get("url") for j in jobs}
    remaining_slots = max(0, max_items - len(jobs))
    for url, title in list(detail_links.items())[:remaining_slots]:
        if url in seen_urls:
            continue
        try:
            parsed = generic_page_job(client.get(url), source, url, title)
            if parsed:
                jobs.append(parsed)
        except Exception:
            continue
    if not jobs and list_errors:
        raise RuntimeError("；".join(list_errors)[:1000])
    return jobs[:max_items]


def feed_items(raw: bytes, source: dict[str, Any]) -> list[dict[str, Any]]:
    root = ET.fromstring(raw)
    jobs = []
    entries = list(root.findall(".//item")) or list(root.findall(".//{http://www.w3.org/2005/Atom}entry"))
    for item in entries:
        def val(*names: str) -> str:
            for name in names:
                node = item.find(name)
                if node is not None:
                    if node.attrib.get("href"): return node.attrib["href"]
                    if node.text: return node.text.strip()
            return ""
        title = clean_html(val("title", "{http://www.w3.org/2005/Atom}title"))
        link = val("link", "{http://www.w3.org/2005/Atom}link")
        desc = clean_html(val("description", "content:encoded", "{http://www.w3.org/2005/Atom}content", "{http://www.w3.org/2005/Atom}summary"))
        if not title or not link: continue
        job = blank_job(source, link)
        company_split = re.split(r"\s+(?:at|@|[-–—])\s+", title, maxsplit=1, flags=re.I)
        job["title"] = company_split[0]
        if len(company_split) > 1: job["company"] = company_split[1]
        job["description"] = desc
        job["published_at"] = parse_date(val("pubDate", "{http://www.w3.org/2005/Atom}updated", "{http://purl.org/dc/elements/1.1/}date"))
        job.update(parse_salary_text(desc + " " + title))
        jobs.append(job)
    return jobs


def connector_generic_feed(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    return feed_items(client.get(source["feed_url"]), source)[:max_items]


def connector_cryptojobslist(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    key = os.getenv("CRYPTOJOBSLIST_API_KEY")
    if not key:
        jobs = connector_generic_feed(source, client, max_items)
        if jobs:
            return jobs
        fallback = dict(source)
        fallback["list_urls"] = [source["homepage"]]
        fallback["job_path_patterns"] = [r"/jobs/"]
        return connector_public_html(fallback, client, max_items)
    payload = client.json(source["api_url"] + "?remote=true&limit=" + str(min(100, max_items)), {"x-api-key": key})
    jobs = []
    for obj in payload.get("jobs", []):
        job = blank_job(source, as_text(obj.get("canonicalURL") or obj.get("url")))
        job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("jobTitle") or obj.get("title")),
                    "company": as_text(obj.get("companyName") or obj.get("company")), "description": clean_html(obj.get("description")),
                    "location": as_text(obj.get("jobLocation") or obj.get("location")) or "Remote",
                    "remote": bool(obj.get("remote")), "employment_type": as_text(obj.get("employmentType")) or "未公开",
                    "published_at": parse_date(obj.get("publishedAt")), "skills": obj.get("tags") or []})
        salary_text = as_text(obj.get("salary"))
        job.update(parse_salary_text(salary_text, assume_salary_field=True) or parse_salary_text(job["description"]))
        jobs.append(job)
    return jobs


def connector_remoteok(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    payload = client.json("https://remoteok.com/api")
    jobs = []
    for obj in payload if isinstance(payload, list) else []:
        if not obj.get("position"): continue
        job = blank_job(source, as_text(obj.get("url") or obj.get("apply_url")))
        job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("position")), "company": as_text(obj.get("company")),
                    "description": clean_html(obj.get("description")), "location": as_text(obj.get("location")) or "Remote",
                    "remote": True, "published_at": parse_date(obj.get("date") or obj.get("epoch")),
                    "salary_min": number(obj.get("salary_min")), "salary_max": number(obj.get("salary_max")),
                    "salary_currency": "USD" if obj.get("salary_min") else None, "salary_period": "year" if obj.get("salary_min") else None,
                    "salary_original": f"USD {obj.get('salary_min')}-{obj.get('salary_max')} / year" if obj.get("salary_min") else "未公开"})
        jobs.append(job)
    return jobs[:max_items]


def connector_remotive(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    payload = client.json("https://remotive.com/api/remote-jobs?limit=" + str(max_items))
    jobs = []
    for obj in payload.get("jobs", []):
        job = blank_job(source, as_text(obj.get("url")))
        job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("title")), "company": as_text(obj.get("company_name")),
                    "description": clean_html(obj.get("description")), "location": as_text(obj.get("candidate_required_location")) or "Remote",
                    "remote": True, "employment_type": as_text(obj.get("job_type")) or "未公开",
                    "published_at": parse_date(obj.get("publication_date")), "salary_original": as_text(obj.get("salary")) or "未公开"})
        job.update(parse_salary_text(job["salary_original"], assume_salary_field=True))
        jobs.append(job)
    return jobs


def connector_jobicy(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    payload = client.json("https://jobicy.com/api/v2/remote-jobs?count=" + str(min(100, max_items)))
    jobs = []
    for obj in payload.get("jobs", []):
        job = blank_job(source, as_text(obj.get("url")))
        job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("jobTitle")), "company": as_text(obj.get("companyName")),
                    "description": clean_html(obj.get("jobDescription")), "location": as_text(obj.get("jobGeo")) or "Remote",
                    "remote": True, "employment_type": as_text(obj.get("jobType")) or "未公开",
                    "published_at": parse_date(obj.get("pubDate")), "salary_min": number(obj.get("salaryMin")),
                    "salary_max": number(obj.get("salaryMax")), "salary_currency": as_text(obj.get("salaryCurrency")).upper() or None,
                    "salary_period": as_text(obj.get("salaryPeriod")).lower() or None,
                    "salary_original": (f"{obj.get('salaryCurrency')} {obj.get('salaryMin')}-{obj.get('salaryMax')} / {obj.get('salaryPeriod')}" if obj.get("salaryMin") is not None else "未公开")})
        jobs.append(job)
    return jobs


def connector_himalayas(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    payload = client.json("https://himalayas.app/jobs/api?limit=" + str(min(20, max_items)))
    objects = payload.get("jobs") or payload.get("data") or []
    jobs = []
    for obj in objects:
        company = obj.get("company") if isinstance(obj.get("company"), dict) else {}
        job = blank_job(source, as_text(obj.get("applicationLink") or obj.get("url")))
        job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("title")),
                    "company": as_text(company.get("name") or obj.get("companyName")), "description": clean_html(obj.get("description")),
                    "location": as_text(obj.get("locationRestrictions") or obj.get("location")) or "Remote",
                    "remote": True, "timezone_original": as_text(obj.get("timezoneRestriction")) or "未公开",
                    "employment_type": as_text(obj.get("employmentType")) or "未公开", "published_at": parse_date(obj.get("publishedAt")),
                    "salary_min": number(obj.get("minSalary") or obj.get("salaryMin")), "salary_max": number(obj.get("maxSalary") or obj.get("salaryMax")),
                    "salary_currency": as_text(obj.get("currency") or obj.get("salaryCurrency")).upper() or None,
                    "salary_period": as_text(obj.get("salaryPeriod") or obj.get("salaryInterval") or obj.get("payPeriod")).lower() or None})
        jobs.append(job)
    return jobs


def connector_arbeitnow(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    payload = client.json("https://www.arbeitnow.com/api/job-board-api")
    jobs = []
    for obj in payload.get("data", []):
        if not obj.get("remote"): continue
        job = blank_job(source, as_text(obj.get("url")))
        job.update({"job_id": as_text(obj.get("slug")), "title": as_text(obj.get("title")), "company": as_text(obj.get("company_name")),
                    "description": clean_html(obj.get("description")), "location": as_text(obj.get("location")) or "Remote",
                    "remote": True, "employment_type": as_text(obj.get("job_types")) or "未公开", "published_at": parse_date(obj.get("created_at"))})
        jobs.append(job)
    return jobs[:max_items]


def connector_hn_whoishiring(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    query = urllib.parse.quote("Ask HN: Who is hiring?")
    search = client.json(f"https://hn.algolia.com/api/v1/search_by_date?query={query}&tags=story&hitsPerPage=10")
    story = next((x for x in search.get("hits", []) if "who is hiring" in as_text(x.get("title")).lower()), None)
    if not story: return []
    item = client.json("https://hn.algolia.com/api/v1/items/" + as_text(story.get("objectID")))
    jobs = []
    for child in item.get("children", [])[:max_items * 2]:
        desc = clean_html(child.get("text"))
        if not any(word in desc.lower() for word in REMOTE_WORDS): continue
        first = re.split(r"[|\n]", desc, maxsplit=1)[0][:200]
        url_match = re.search(r"https?://[^\s<]+", desc)
        url = url_match.group(0).rstrip(".,)") if url_match else f"https://news.ycombinator.com/item?id={child.get('id')}"
        job = blank_job(source, url)
        job.update({"job_id": as_text(child.get("id")), "title": first or "HN remote opportunity", "description": desc,
                    "location": "Remote（范围待核验）", "remote": True, "published_at": parse_date(child.get("created_at")), "confidence": "low"})
        jobs.append(job)
    return jobs[:max_items]


def connector_adzuna(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    app_id, key = os.getenv("ADZUNA_APP_ID"), os.getenv("ADZUNA_APP_KEY")
    if not app_id or not key: raise RuntimeError("缺少 ADZUNA_APP_ID/ADZUNA_APP_KEY")
    jobs = []
    for country in ("us", "gb", "ca", "de", "fr", "nl"):
        params = urllib.parse.urlencode({"app_id": app_id, "app_key": key, "results_per_page": min(50, max_items), "what_or": "rust golang c++ web3 blockchain", "where": "remote"})
        payload = client.json(f"https://api.adzuna.com/v1/api/jobs/{country}/search/1?{params}")
        for obj in payload.get("results", []):
            job = blank_job(source, as_text(obj.get("redirect_url")))
            loc = obj.get("location") or {}
            job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("title")),
                        "company": as_text((obj.get("company") or {}).get("display_name")), "description": clean_html(obj.get("description")),
                        "location": as_text(loc.get("display_name")), "remote": "remote" in (as_text(obj.get("title")) + as_text(obj.get("description"))).lower(),
                        "published_at": parse_date(obj.get("created")), "salary_min": number(obj.get("salary_min")), "salary_max": number(obj.get("salary_max")),
                        "salary_currency": "GBP" if country == "gb" else ("EUR" if country in {"de", "fr", "nl"} else ("CAD" if country == "ca" else "USD")), "salary_period": "year"})
            jobs.append(job)
    return jobs[:max_items]


def ats_boards(provider: str) -> list[dict[str, Any]]:
    boards = load_json(REFERENCES / "ats-boards.json").get("boards", [])
    return [board for board in boards if board.get("provider") == provider and board.get("token")]


def connector_greenhouse(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    jobs = []
    for board in ats_boards("greenhouse"):
        payload = client.json(f"https://boards-api.greenhouse.io/v1/boards/{urllib.parse.quote(board['token'])}/jobs?content=true")
        for obj in payload.get("jobs", []):
            job = blank_job(source, as_text(obj.get("absolute_url")))
            job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("title")), "company": board.get("company", board["token"]),
                        "description": clean_html(obj.get("content")), "location": as_text((obj.get("location") or {}).get("name")),
                        "published_at": parse_date(obj.get("updated_at"))})
            jobs.append(job)
            if len(jobs) >= max_items: return jobs
    return jobs


def connector_lever(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    jobs = []
    for board in ats_boards("lever"):
        payload = client.json(f"https://api.lever.co/v0/postings/{urllib.parse.quote(board['token'])}?mode=json")
        for obj in payload if isinstance(payload, list) else []:
            salary = obj.get("salaryRange") or {}
            categories = obj.get("categories") or {}
            job = blank_job(source, as_text(obj.get("hostedUrl") or obj.get("applyUrl")))
            job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("text")), "company": board.get("company", board["token"]),
                        "description": clean_html(obj.get("descriptionPlain") or obj.get("description")),
                        "location": as_text(categories.get("location")), "remote": as_text(obj.get("workplaceType")).lower() == "remote",
                        "employment_type": as_text(categories.get("commitment")) or "未公开",
                        "salary_min": number(salary.get("min")), "salary_max": number(salary.get("max")),
                        "salary_currency": as_text(salary.get("currency")).upper() or None,
                        "salary_period": as_text(salary.get("interval")).lower() or None})
            jobs.append(job)
            if len(jobs) >= max_items: return jobs
    return jobs


def connector_ashby(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    jobs = []
    for board in ats_boards("ashby"):
        payload = client.json(f"https://api.ashbyhq.com/posting-api/job-board/{urllib.parse.quote(board['token'])}?includeCompensation=true")
        for obj in payload.get("jobs", []):
            job = blank_job(source, as_text(obj.get("jobUrl") or obj.get("applyUrl")))
            job.update({"job_id": as_text(obj.get("id")), "title": as_text(obj.get("title")), "company": board.get("company", board["token"]),
                        "description": clean_html(obj.get("descriptionPlain") or obj.get("descriptionHtml")),
                        "location": as_text(obj.get("location")), "remote": bool(obj.get("isRemote")),
                        "employment_type": as_text(obj.get("employmentType")) or "未公开",
                        "published_at": parse_date(obj.get("publishedAt")), "salary_original": as_text(obj.get("compensation")) or "未公开"})
            job.update(parse_salary_text(job["salary_original"], assume_salary_field=True))
            jobs.append(job)
            if len(jobs) >= max_items: return jobs
    return jobs


def connector_smartrecruiters(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    jobs = []
    for board in ats_boards("smartrecruiters"):
        company_id = urllib.parse.quote(board["token"])
        payload = client.json(f"https://api.smartrecruiters.com/v1/companies/{company_id}/postings?limit=100")
        for item in payload.get("content", []):
            location = item.get("location") or {}
            detail = item
            try:
                detail = client.json(f"https://api.smartrecruiters.com/v1/companies/{company_id}/postings/{item['id']}")
            except Exception:
                pass
            sections = detail.get("jobAd", {}).get("sections", {})
            description = " ".join(clean_html(section.get("text")) for section in sections.values() if isinstance(section, dict))
            job = blank_job(source, as_text(detail.get("ref") or item.get("ref")))
            job.update({"job_id": as_text(item.get("id")), "title": as_text(item.get("name")), "company": board.get("company", board["token"]),
                        "description": description, "location": ", ".join(as_text(location.get(k)) for k in ("city", "region", "country") if location.get(k)),
                        "remote": bool(location.get("remote")), "employment_type": as_text((item.get("typeOfEmployment") or {}).get("label")),
                        "published_at": parse_date(item.get("releasedDate"))})
            jobs.append(job)
            if len(jobs) >= max_items: return jobs
    return jobs


def connector_usajobs(source: dict[str, Any], client: HttpClient, max_items: int) -> list[dict[str, Any]]:
    key, email = os.getenv("USAJOBS_API_KEY"), os.getenv("USAJOBS_EMAIL")
    if not key or not email:
        raise RuntimeError("缺少 USAJOBS_API_KEY/USAJOBS_EMAIL")
    headers = {"Authorization-Key": key, "User-Agent": email, "Host": "data.usajobs.gov"}
    params = urllib.parse.urlencode({"Keyword": "remote software engineer", "ResultsPerPage": min(100, max_items), "WhoMayApply": "Public"})
    payload = client.json("https://data.usajobs.gov/api/search?" + params, headers)
    jobs = []
    for result in payload.get("SearchResult", {}).get("SearchResultItems", []):
        obj = result.get("MatchedObjectDescriptor") or {}
        details = obj.get("UserArea", {}).get("Details", {})
        remuneration = (obj.get("PositionRemuneration") or [{}])[0]
        job = blank_job(source, as_text(obj.get("PositionURI")))
        job.update({"job_id": as_text(obj.get("PositionID")), "title": as_text(obj.get("PositionTitle")),
                    "company": as_text(obj.get("OrganizationName") or obj.get("DepartmentName")),
                    "description": clean_html(" ".join((as_text(details.get("JobSummary")), as_text(details.get("MajorDuties")), as_text(details.get("Requirements"))))),
                    "location": as_text([loc.get("LocationName") for loc in obj.get("PositionLocation", [])]),
                    "remote": "remote" in as_text(obj.get("PositionLocationDisplay")).lower(),
                    "employment_type": as_text(obj.get("PositionSchedule")), "published_at": parse_date(obj.get("PublicationStartDate")),
                    "expires_at": parse_date(obj.get("ApplicationCloseDate")), "salary_min": number(remuneration.get("MinimumRange")),
                    "salary_max": number(remuneration.get("MaximumRange")), "salary_currency": "USD",
                    "salary_period": as_text(remuneration.get("RateIntervalCode")).lower() or "year"})
        jobs.append(job)
    return jobs


CONNECTORS = {
    "public_html": connector_public_html, "generic_feed": connector_generic_feed,
    "cryptojobslist": connector_cryptojobslist, "remoteok": connector_remoteok,
    "remotive": connector_remotive, "jobicy": connector_jobicy, "himalayas": connector_himalayas,
    "arbeitnow": connector_arbeitnow, "weworkremotely": connector_generic_feed,
    "hn_whoishiring": connector_hn_whoishiring, "adzuna": connector_adzuna,
    "greenhouse": connector_greenhouse, "lever": connector_lever, "ashby": connector_ashby,
    "smartrecruiters": connector_smartrecruiters, "usajobs": connector_usajobs,
}


def source_feed_defaults(source: dict[str, Any]) -> None:
    if source.get("connector") == "weworkremotely" and not source.get("feed_url"):
        source["feed_url"] = "https://weworkremotely.com/categories/remote-programming-jobs.rss"


def dedupe_jobs(jobs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    best: dict[str, dict[str, Any]] = {}
    for job in jobs:
        url_key = canonical_url(job.get("url", ""))
        title_key = re.sub(r"[^a-z0-9]+", "", (job.get("title", "") + job.get("company", "")).lower())
        key = url_key if url_key else title_key
        existing = best.get(key)
        completeness = sum(job.get(k) not in (None, "", "未公开", [], "unknown") for k in ("description", "salary_min", "location", "published_at", "timezone_original"))
        if existing:
            old = sum(existing.get(k) not in (None, "", "未公开", [], "unknown") for k in ("description", "salary_min", "location", "published_at", "timezone_original"))
            if completeness > old: best[key] = job
        else:
            best[key] = job
    # 第二道软去重：同公司同标题即使聚合 URL 不同也只保留字段最完整、来源优先级最高的一条。
    soft: dict[str, dict[str, Any]] = {}
    for job in best.values():
        key = re.sub(r"[^a-z0-9]+", "", (job["title"] + "|" + job["company"]).lower())
        if len(key) < 8: key = job["url"]
        current = soft.get(key)
        rank = (job.get("source_priority", 0), len(job.get("description", "")), bool(job.get("salary_min")))
        if current is None or rank > (current.get("source_priority", 0), len(current.get("description", "")), bool(current.get("salary_min"))):
            soft[key] = job
    return list(soft.values())


def write_jsonl(path: Path, rows: Iterable[dict[str, Any]]) -> None:
    atomic_write(path, "".join(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n" for row in rows))


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    with path.open("r", encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            if not line.strip(): continue
            try: rows.append(json.loads(line))
            except json.JSONDecodeError as exc: raise ValueError(f"{path}:{line_no}: {exc}") from exc
    return rows


def salary_display(job: dict[str, Any]) -> str:
    if job.get("salary_usd_annual_min") is None:
        if job.get("salary_min") is not None and not job.get("salary_period"):
            return f"{job.get('salary_original', '金额已公开')}（周期未公开，不参与年薪排序）"
        return "未公开（异常原始值已隔离）" if job.get("salary_rejected_reason") else "未公开"
    value = f"USD {job['salary_usd_annual_min']:,.0f}–{job['salary_usd_annual_max']:,.0f}/年；约 RMB {job['salary_cny_annual_min']:,.0f}–{job['salary_cny_annual_max']:,.0f}/年"
    if job.get("salary_kind") == "estimated":
        return "聚合站估算：" + value + "（不参与收入加分）"
    label = "结构化明确" if job.get("salary_kind") == "explicit_structured" else "正文明确"
    return value + f"（{label}）"


FOCUS_ROLE = re.compile(
    r"(?i)software|engineer|developer|backend|systems?|platform|infrastructure|devops|sre|"
    r"reliability|security|quant|compiler|database|network|embedded|blockchain|protocol|kernel"
)
LEARNING_CATEGORIES = {
    "foundation": {"English", "System Design", "Networking", "Testing", "SQL", "PostgreSQL", "Backend", "Distributed Systems"},
    "platform": {"Docker", "Kubernetes", "AWS", "GCP", "Azure", "Terraform", "Observability", "SRE", "DevOps", "Kafka", "NATS", "Redis", "ClickHouse"},
    "domain": {"Web3", "Ethereum", "Solana", "DeFi", "Smart Contracts", "Cryptography", "Zero Knowledge", "RPC/Nodes", "Substrate", "Cosmos SDK", "Trading Systems"},
}
LEARNING_GUIDE = {
    "English": ("把技术经历写成英文问题—行动—结果；练习 3 分钟项目讲解", "完成英文简历、README 和 10 题口语录音，自检无含糊主语", "https://developers.google.com/tech-writing"),
    "System Design": ("容量估算、API/数据模型、缓存、一致性、故障与可观测性", "为作品画架构图并写 3 个失败场景及恢复方案", "https://github.com/donnemartin/system-design-primer"),
    "Networking": ("TCP/HTTP、TLS、DNS、负载均衡、超时重试与背压", "用抓包或指标证明一次请求从客户端到服务端的完整路径", "https://beej.us/guide/bgnet/"),
    "Web3": ("区块、交易、签名、节点、RPC、索引器与链重组", "从真实测试网同步并查询数据，能解释 reorg 后如何纠正", "https://ethereum.org/en/developers/docs/"),
    "Ethereum": ("EVM、交易生命周期、事件日志、JSON-RPC 与 gas", "实现事件索引和断点续扫，覆盖链重组测试", "https://ethereum.org/en/developers/docs/"),
    "Solana": ("账户模型、交易、程序、RPC、Anchor 与性能约束", "读取测试网账户并实现可恢复的交易/事件索引", "https://solana.com/docs"),
    "Kubernetes": ("Pod/Deployment/Service、探针、资源限制、滚动发布", "本地集群一键部署，故意杀 Pod 后服务自动恢复", "https://kubernetes.io/docs/tutorials/"),
    "Docker": ("多阶段构建、最小镜像、非 root、健康检查", "镜像可复现且小于基础开发镜像，安全扫描无高危项", "https://docs.docker.com/get-started/"),
    "AWS": ("IAM、VPC、计算、对象存储、数据库与成本边界", "用 IaC 部署最小环境并附月成本估算和删除步骤", "https://docs.aws.amazon.com/whitepapers/latest/aws-overview/"),
    "Terraform": ("state、module、plan/apply、漂移与密钥隔离", "从空账号可复建环境，plan 二次执行无漂移", "https://developer.hashicorp.com/terraform/tutorials"),
    "Observability": ("日志、指标、trace、SLO、告警与排障路径", "注入慢请求和错误，仪表盘能在 5 分钟内定位根因", "https://opentelemetry.io/docs/"),
    "Python": ("脚本、异步 I/O、数据处理、测试与类型提示", "写一个可重试、限速、带测试的数据采集器", "https://docs.python.org/3/tutorial/"),
    "PostgreSQL": ("索引、事务隔离、执行计划、迁移与备份恢复", "为百万行数据优化查询并保存 EXPLAIN 前后对比", "https://www.postgresql.org/docs/current/tutorial.html"),
    "SQL": ("连接、聚合、窗口函数、索引与事务", "完成 20 个查询题并解释每个执行计划", "https://www.postgresql.org/docs/current/tutorial-sql.html"),
    "Security": ("威胁建模、鉴权、密钥、依赖和供应链安全", "提交威胁模型并修复至少 3 个主动注入的问题", "https://owasp.org/www-project-application-security-verification-standard/"),
    "Smart Contracts": ("状态、权限、重入、精度、升级与测试", "完成测试网合约和攻击用例，静态分析无高危项", "https://docs.soliditylang.org/"),
    "Cryptography": ("哈希、签名、密钥交换、随机数与常见误用", "用成熟库实现签名验证，并解释为什么不自造算法", "https://cryptobook.nakov.com/"),
    "TypeScript": ("类型系统、Node 运行时、API 客户端与测试", "为作品补一个类型安全的 CLI 或最小控制台", "https://www.typescriptlang.org/docs/handbook/intro.html"),
    "React": ("组件、状态、请求、错误边界和可访问性", "只做能展示核心数据的最小界面，不挤占后端主线", "https://react.dev/learn"),
}


def profile_skill_set(profile: dict[str, Any]) -> set[str]:
    skills = {item.get("name", "") for item in profile.get("languages", [])}
    skills.update(profile.get("other_skills", []))
    return {x for x in skills if x}


def is_focus_job(job: dict[str, Any]) -> bool:
    return bool({"Rust", "C++", "Go"} & set(job.get("skills", []))) or bool(FOCUS_ROLE.search(job.get("title", "")))


def learning_statistics(jobs: list[dict[str, Any]], profile: dict[str, Any]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    candidates = [j for j in jobs if j.get("eligibility") != "ineligible" and is_focus_job(j)]
    known = profile_skill_set(profile)
    by_skill: dict[str, dict[str, Any]] = {}
    for job in candidates:
        for skill in set(job.get("skills", [])) - known:
            stat = by_skill.setdefault(skill, {"technology": skill, "jobs": 0, "required": 0, "preferred": 0,
                                                "mentioned": 0, "weighted": 0.0, "examples": [], "co": {}})
            level = job.get("skill_requirements", {}).get(skill, "mentioned")
            stat["jobs"] += 1
            stat[level] += 1
            stat["weighted"] += {"required": 3.0, "preferred": 1.5, "mentioned": .5}[level] * (1 if job.get("eligibility") == "eligible" else .6)
            if len(stat["examples"]) < 3:
                stat["examples"].append(job.get("title", "未命名岗位"))
            for peer in set(job.get("skills", [])) - {skill}:
                stat["co"][peer] = stat["co"].get(peer, 0) + 1
    rows = []
    for stat in by_skill.values():
        peers = sorted(stat.pop("co").items(), key=lambda x: (-x[1], x[0]))[:3]
        stat["cooccurs"] = ", ".join(name for name, _ in peers) or "—"
        stat["coverage"] = round(100 * stat["jobs"] / max(1, len(candidates)), 1)
        stat["weighted"] = round(stat["weighted"], 1)
        rows.append(stat)
    rows.sort(key=lambda x: (-x["weighted"], -x["required"], -x["jobs"], x["technology"]))
    return candidates, rows


def build_learning_roadmap(jobs: list[dict[str, Any]], profile: dict[str, Any]) -> str:
    candidates, gaps = learning_statistics(jobs, profile)
    primary_counts = {lang: sum(lang in job.get("skills", []) for job in candidates) for lang in ("Rust", "C++", "Go")}
    track_order = sorted(primary_counts, key=lambda x: (-primary_counts[x], x))
    top = gaps[:10]
    top_by_category: dict[str, list[str]] = {}
    for category, members in LEARNING_CATEGORIES.items():
        top_by_category[category] = [row["technology"] for row in gaps if row["technology"] in members][:4]
    unclassified = [row["technology"] for row in gaps if not any(row["technology"] in values for values in LEARNING_CATEGORIES.values())][:3]
    foundation = top_by_category["foundation"] or unclassified or ["System Design"]
    platform = top_by_category["platform"] or ["Docker", "Observability"]
    domain = top_by_category["domain"] or ["Distributed Systems"]
    main = track_order[0]
    projects = {
        "Rust": "Rust 异步 RPC/链上索引器：断点续扫、重组恢复、PostgreSQL、指标与容器部署",
        "Go": "Go 并发后端服务：限流、队列、PostgreSQL、OpenTelemetry 与 Kubernetes 部署",
        "C++": "C++20 低延迟订单簿：网络输入、基准测试、故障恢复、可观测性与 Python 回放工具",
    }
    unknown_years = [x.get("name") for x in profile.get("languages", []) if x.get("years") is None]
    lines = [
        "# 个性化求职学习路线", "",
        f"生成时间：{now_iso()}；画像：{profile.get('profile_name', 'default')}。这份路线只统计 **{len(candidates)}** 个与软件工程及 Rust/C++/Go 相关、且未被判定为地域不符合的岗位；不会用销售、法务、设计岗位的词频带偏学习方向。", "",
        "## 先说结论", "",
        f"主攻顺序：**{' → '.join(f'{lang}（{primary_counts[lang]} 岗）' for lang in track_order)}**。先用 {main} 做一件能部署、能观测、能解释失败恢复的作品；另外两门语言只做面试保温，不要三条主线同时开工。", "",
    ]
    if unknown_years:
        lines += [f"> 画像里 {', '.join(unknown_years)} 的年限/熟练度仍是未知。因此第 0 周是强制分级测试；通过验收的基础项直接跳过，没通过再补。路线不会假装知道你的水平。", ""]
    lines += [
        "## 岗位证据驱动的技能优先级", "",
        "|优先|技能|涉及岗位|明确必需|加分项|仅提及|样本覆盖|常一起出现|岗位例子|", "|---:|---|---:|---:|---:|---:|---:|---|---|",
    ]
    for rank, row in enumerate(top, 1):
        examples = "；".join(x.replace("|", "/") for x in row["examples"][:2])
        lines.append(f"|{rank}|{row['technology']}|{row['jobs']}|{row['required']}|{row['preferred']}|{row['mentioned']}|{row['coverage']:.1f}%|{row['cooccurs']}|{examples}|")
    lines += [
        "", "读表规则：先看“明确必需”，再看加分项；“仅提及”只当背景信号。不要因为某个词出现很多次，就立刻买课。", "",
        "## 12 周执行表（每周 15 小时）", "",
        "|周|学习与实践|必须交付|通过标准|", "|---:|---|---|---|",
        "|0|Rust/C++/Go 各做 60–90 分钟限时任务；英文讲解旧项目；复核目标岗位 20 条|能力清单、英文自我介绍录音、20 条岗位证据表|能明确写出会/不会/需查资料；修正画像中的年限和等级|",
        f"|1|{', '.join(foundation[:2])}|2 篇故障/设计笔记 + 最小实验|不是复述概念；必须包含测量数据、失败现象和修复|",
        f"|2|{', '.join(foundation[2:4] or foundation[:2])}|数据模型、API 和容量估算文档|能回答一致性、索引、超时、重试、幂等取舍|",
        f"|3|{', '.join(domain[:2])}|从公开测试环境读取真实数据的 CLI|断网/限流后可恢复，不重复或漏处理|",
        f"|4|{', '.join(domain[2:4] or domain[:2])}|领域数据流图 + 10 个失败案例测试|能解释安全边界、最终一致性和回滚策略|",
        f"|5|{', '.join(platform[:2])}|容器化服务 + 本地一键启动|非 root、健康检查、配置与密钥分离|",
        f"|6|{', '.join(platform[2:4] or platform[:2])}|部署脚本、指标、trace 和告警|注入慢请求/错误后 5 分钟内定位根因|",
        f"|7|主项目开工：{projects[main]}|README、架构图、里程碑和可运行骨架|陌生人照 README 能在 15 分钟内跑起来|",
        "|8|完成核心数据路径和持久化|端到端演示、单元/集成/属性测试|核心路径有基准数据，失败可重试且幂等|",
        "|9|可靠性与性能周|压测报告、故障注入、恢复演示|报告包含 p50/p95/p99、资源占用和瓶颈结论|",
        "|10|安全、可观测性和发布|威胁模型、仪表盘、版本化发布包|至少修复 3 个主动注入问题；新机器可复现部署|",
        "|11|面试周：语言、系统设计、项目深挖、英文沟通|30 道题复盘 + 5 次录音模拟面试|每题都能给证据和取舍，不用“应该、可能”糊弄|",
        "|12|投递周：每天 3–5 个高匹配岗位并复盘|岗位—证据—简历版本—结果看板|每次拒绝/无回复都转成一个可验证改动，禁止海投同一简历|",
        "", "每周时间配比：学习 4 小时、编码 7 小时、测试/文档 2 小时、英文与投递 2 小时。若每周只有 8 小时，按同一顺序延长到 20–22 周，不删测试和交付物。", "",
        "## 三条作品线怎么选", "",
    ]
    for lang in track_order:
        marker = "（本轮主线）" if lang == main else "（保温/备选）"
        lines.append(f"- **{lang}{marker}**：{projects[lang]}。岗位样本 {primary_counts[lang]} 条。")
    lines += ["", "## 前 8 个技能的学习卡", ""]
    for row in top[:8]:
        skill = row["technology"]
        learn, proof, url = LEARNING_GUIDE.get(skill, (f"先读官方概览，再做一个能接入主项目的 {skill} 最小实验", "实验必须有自动测试、失败案例和一页取舍说明", ""))
        lines += [f"### {skill}", "", f"- 为什么学：{row['jobs']} 个相关岗位，其中 {row['required']} 个明确要求、{row['preferred']} 个作为加分项。", f"- 学什么：{learn}。", f"- 验收：{proof}。"]
        if url:
            lines.append(f"- 起点资料：{url}")
        lines.append("")
    lines += [
        "## 每周复盘与停止规则", "",
        "- 只记录可验证证据：提交、测试、基准、部署链接、英文录音、面试反馈。看完视频不算完成。",
        "- 连续两周没有交付，下一周不准加新技术；缩小项目范围直到能发布。",
        "- 某技能若只在“仅提及”出现、且 20 个目标岗位中没有明确要求，降为按需查阅。",
        "- 每两周重新扫描一次岗位；只有当必需岗位数持续上升，才调整路线优先级。",
        "- 薪资未公开或聚合站估算不影响是否学习；学习路线以要求证据为准，薪资只用于已验证岗位之间排序。", "",
    ]
    return "\n".join(lines)


def build_quality_report(jobs: list[dict[str, Any]]) -> str:
    kinds: dict[str, int] = {}
    for job in jobs:
        kinds[job.get("salary_kind", "unknown")] = kinds.get(job.get("salary_kind", "unknown"), 0) + 1
    rejected = [j for j in jobs if j.get("salary_rejected_reason")]
    downgraded = [j for j in jobs if "聚合站远程/Worldwide" in j.get("eligibility_reason", "")]
    clusters: dict[tuple[str, float, float], int] = {}
    for job in jobs:
        if job.get("salary_usd_annual_min") is None:
            continue
        key = (job.get("source", "未公开"), job["salary_usd_annual_min"], job["salary_usd_annual_max"])
        clusters[key] = clusters.get(key, 0) + 1
    repeated = sorted(((count, key) for key, count in clusters.items() if count >= 5), reverse=True)
    lines = [
        "# 数据质量审计", "", f"生成时间：{now_iso()}。本文件解释哪些字段能信、哪些只适合当线索。", "",
        "## 薪资可信度", "", "|类型|数量|是否参与收入加分|", "|---|---:|---|",
        f"|招聘页结构化明确|{kinds.get('explicit_structured', 0)}|是|",
        f"|招聘正文明确|{kinds.get('explicit_text', 0)}|是|",
        f"|聚合站估算|{kinds.get('estimated', 0)}|否|",
        f"|未公开/已隔离|{kinds.get('unknown', 0)}|否|", "",
        f"明显不成立的薪资共 **{len(rejected)}** 条，已保留原文和原因，但清空折算值，不参与排序。聚合站国际资格被降为待确认 **{len(downgraded)}** 条。", "",
        "## 重复薪资区间", "", "|来源|区间|重复数|处理|", "|---|---|---:|---|",
    ]
    if repeated:
        for count, (source, low, high) in repeated[:20]:
            sample = next(j for j in jobs if j.get("source") == source and j.get("salary_usd_annual_min") == low and j.get("salary_usd_annual_max") == high)
            treatment = "仅作估算" if sample.get("salary_kind") == "estimated" else "保留明确值；建议抽样核验"
            lines.append(f"|{source}|USD {low:,.0f}–{high:,.0f}/年|{count}|{treatment}|")
    else:
        lines.append("|—|没有达到 5 条的重复区间|0|—|")
    lines += ["", "## 被隔离样例", "", "|岗位|来源|原始值|原因|", "|---|---|---|---|"]
    for job in rejected[:20]:
        lines.append(f"|{job['title'].replace('|', '/')}|{job['source']}|{str(job.get('salary_original', '')).replace('|', '/')[:100]}|{job['salary_rejected_reason'].replace('|', '/')}|")
    if not rejected:
        lines.append("|—|—|—|本轮无明显异常值|")
    lines += ["", "> 原则：招聘正文没写就显示未公开；聚合站估算永远不包装成招聘方承诺；可从中国受雇必须有原招聘正文证据。", ""]
    return "\n".join(lines)


def report_outputs(jobs: list[dict[str, Any]], out: Path, profile: dict[str, Any], source_reports: list[dict[str, Any]], rate_source: str) -> None:
    jobs.sort(key=lambda j: (j.get("eligibility") == "eligible", j.get("score", 0),
                             j.get("salary_kind") in {"explicit_structured", "explicit_text"},
                             j.get("salary_usd_annual_max") or 0), reverse=True)
    write_jsonl(out / "jobs.jsonl", jobs)
    columns = ["score", "eligibility", "title", "company", "source", "location", "remote_scope", "experience_min_years",
               "weekly_hours", "contract_duration", "timezone_original", "beijing_hours", "salary_original",
               "salary_kind", "salary_confidence", "salary_evidence", "salary_rejected_reason",
               "salary_usd_annual_min", "salary_usd_annual_max", "difficulty_label", "skills", "missing_skills", "published_at", "url"]
    with (out / "ranked.csv").open("w", encoding="utf-8-sig", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=columns, extrasaction="ignore")
        writer.writeheader()
        for job in jobs:
            row = dict(job); row["skills"] = ", ".join(job.get("skills", [])); row["missing_skills"] = ", ".join(job.get("missing_skills", [])); writer.writerow(row)
    counts: dict[str, dict[str, int]] = {}
    eligible = [j for j in jobs if j.get("eligibility") != "ineligible"]
    for job in eligible:
        for skill in job.get("skills", []):
            stat = counts.setdefault(skill, {"jobs": 0, "required": 0})
            stat["jobs"] += 1
            if skill in job.get("missing_skills", []): stat["required"] += 1
    tech_rows = sorted(((skill, v["jobs"], v["required"]) for skill, v in counts.items()), key=lambda x: (-x[1], x[0]))
    with (out / "tech_frequency.csv").open("w", encoding="utf-8-sig", newline="") as handle:
        w = csv.writer(handle); w.writerow(["technology", "job_count", "learning_gap_count"]); w.writerows(tech_rows)
    with (out / "skill_gap.csv").open("w", encoding="utf-8-sig", newline="") as handle:
        w = csv.writer(handle); w.writerow(["technology", "missing_in_jobs", "priority"])
        for skill, total, missing in sorted(tech_rows, key=lambda x: (-x[2], -x[1])):
            w.writerow([skill, missing, "高" if missing >= max(2, len(eligible) * .2) else "中"])
    write_wordcloud(out / "wordcloud.svg", tech_rows)
    write_jsonl(out / "source_status.jsonl", source_reports)
    candidates, gap_rows = learning_statistics(jobs, profile)
    with (out / "learning_backlog.csv").open("w", encoding="utf-8-sig", newline="") as handle:
        columns = ["rank", "technology", "weighted_priority", "job_count", "required_jobs", "preferred_jobs", "mentioned_jobs", "coverage_percent", "cooccurs", "examples"]
        writer = csv.DictWriter(handle, fieldnames=columns)
        writer.writeheader()
        for rank, row in enumerate(gap_rows, 1):
            writer.writerow({"rank": rank, "technology": row["technology"], "weighted_priority": row["weighted"],
                             "job_count": row["jobs"], "required_jobs": row["required"], "preferred_jobs": row["preferred"],
                             "mentioned_jobs": row["mentioned"], "coverage_percent": row["coverage"],
                             "cooccurs": row["cooccurs"], "examples": "；".join(row["examples"])})
    atomic_write(out / "learning-roadmap.md", build_learning_roadmap(jobs, profile) + "\n")
    atomic_write(out / "quality-report.md", build_quality_report(jobs) + "\n")
    recommended = [job for job in jobs if job.get("eligibility") == "eligible"]
    pending = [job for job in jobs if job.get("eligibility") == "unknown"]
    lines = ["# 全球远程岗位报告", "", f"生成时间：{now_iso()} ；画像：{profile.get('profile_name', 'default')}；汇率：{rate_source}。",
             "", "> 未写明的薪资、经验、工时、期限和时区均显示为“未公开”。聚合站估算薪资不参与收入评分；可从中国受雇必须有正文证据。", "",
             f"共保留 **{len(jobs)}** 个去重岗位；其中可从中国申请 {sum(j.get('eligibility') == 'eligible' for j in jobs)} 个，待确认 {sum(j.get('eligibility') == 'unknown' for j in jobs)} 个，明确不符合 {sum(j.get('eligibility') == 'ineligible' for j in jobs)} 个。", "",
             "## 优先岗位", "", "|分数|岗位 / 公司|资格|薪资折算|经验|北京时间|难度|来源|", "|---:|---|---|---|---|---|---|---|"]
    if not recommended:
        lines.append("|—|本轮没有确认可从中国申请的岗位|—|—|—|—|—|—|")
    for job in recommended[:40]:
        title = job["title"].replace("|", "/"); company = job["company"].replace("|", "/")
        exp = "未公开" if job.get("experience_min_years") is None else f"{job['experience_min_years']:g}+ 年"
        lines.append(f"|{job['score']:.1f}|[{title}]({job['url']}) / {company}|{job['eligibility']}：{job['eligibility_reason']}|{salary_display(job)}|{exp}|{job['beijing_hours']}|{job['difficulty_label']}|{job['source']}|")
    lines += ["", "## 远程范围待确认", "", "这些岗位看起来支持远程，但公开信息没有证明人在中国可以受雇。确认前不进入优先推荐。", "",
              "|分数|岗位 / 公司|薪资折算|地点/范围|经验|北京时间|来源|", "|---:|---|---|---|---|---|---|"]
    if not pending:
        lines.append("|—|无|—|—|—|—|—|")
    for job in pending[:30]:
        title = job["title"].replace("|", "/"); company = job["company"].replace("|", "/")
        exp = "未公开" if job.get("experience_min_years") is None else f"{job['experience_min_years']:g}+ 年"
        lines.append(f"|{job['score']:.1f}|[{title}]({job['url']}) / {company}|{salary_display(job)}|{job['location']} / {job['remote_scope']}|{exp}|{job['beijing_hours']}|{job['source']}|")
    lines += ["", "## 学习路线", "", f"已从 {len(candidates)} 个软件工程/Rust/C++/Go 相关岗位生成 `learning-roadmap.md` 和 `learning_backlog.csv`。路线区分明确必需、加分项和仅提及，并给出 12 周交付与验收标准。", "",
              "## 最常见技术栈", "", "|技术|出现岗位数|个人画像未覆盖|", "|---|---:|---:|"]
    lines += [f"|{s}|{n}|{m}|" for s, n, m in tech_rows[:30]]
    lines += ["", "## 来源覆盖", "", "|来源|通道|状态|原始数|有效数|耗时|说明|", "|---|---|---|---:|---:|---:|---|"]
    for status in sorted(source_reports, key=lambda x: (-x.get("priority", 0), x["source"])):
        lines.append(f"|{status['source']}|{status['access']}|{status['status']}|{status.get('count', 0)}|{status.get('accepted_count', 0)}|{status.get('elapsed_ms', 0)} ms|{as_text(status.get('error')).replace('|', '/')}|")
    atomic_write(out / "report.md", "\n".join(lines) + "\n")


def write_wordcloud(path: Path, rows: list[tuple[str, int, int]]) -> None:
    chosen = rows[:36]
    if not chosen:
        chosen = [("暂无数据", 1, 0)]
    maximum = max(row[1] for row in chosen)
    colors = ["#D97757", "#6B7280", "#2563EB", "#059669", "#7C3AED", "#B45309"]
    pieces = ['<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="700" viewBox="0 0 1200 700">', '<rect width="100%" height="100%" fill="#fbfaf7"/>']
    x, y, row_h = 36, 70, 0
    for i, (skill, count, _) in enumerate(chosen):
        size = 18 + int(42 * math.sqrt(count / maximum))
        width = max(80, int(len(skill) * size * .68 + 40))
        if x + width > 1160:
            x, y, row_h = 36, y + row_h + 28, 0
        pieces.append(f'<text x="{x}" y="{y}" font-family="-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif" font-size="{size}" fill="{colors[i % len(colors)]}">{html.escape(skill)} <tspan opacity=".55">{count}</tspan></text>')
        x += width; row_h = max(row_h, size)
    pieces.append("</svg>")
    atomic_write(path, "\n".join(pieces))


def build_search_tasks(sources: list[dict[str, Any]], failed: set[str]) -> list[dict[str, Any]]:
    tasks = []
    for source in sources:
        if source.get("automation") == "implemented" and source["id"] not in failed:
            continue
        queries = QUERY_MATRIX if source.get("priority", 0) >= 85 or source["id"] in failed else QUERY_MATRIX[:4]
        for query in queries:
            full = f"site:{urllib.parse.urlsplit(source['homepage']).netloc} {query}"
            tasks.append({"source_id": source["id"], "source": source["name"], "query": full,
                          "search_url": "https://www.google.com/search?q=" + urllib.parse.quote_plus(full),
                          "homepage": source["homepage"], "reason": "自动通道失败，需网页搜索补齐" if source["id"] in failed else source.get("notes", "search_only")})
    return tasks


def cmd_catalog(args: argparse.Namespace) -> int:
    sources = load_json(REFERENCES / "sources.json")["sources"]
    by_access: dict[str, int] = {}
    by_status: dict[str, int] = {}
    for source in sources:
        by_access[source["access"]] = by_access.get(source["access"], 0) + 1
        by_status[source["automation"]] = by_status.get(source["automation"], 0) + 1
    result = {"reviewed_at": "2026-08-24", "total_sites": len(sources), "by_access": by_access, "by_automation": by_status,
              "priority_sites": [s["name"] for s in sorted(sources, key=lambda x: -x.get("priority", 0))[:12]]}
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


def prepare(profile_path: str) -> tuple[dict[str, Any], dict[str, list[str]]]:
    profile = load_json(Path(profile_path))
    taxonomy_doc = load_json(REFERENCES / "tech-taxonomy.json")
    return profile, taxonomy_doc["technologies"]


def cmd_source_worker(args: argparse.Namespace) -> int:
    """在隔离子进程中抓一个来源；父进程到点可真正终止网络读取。"""
    source = next((item for item in load_json(REFERENCES / "sources.json")["sources"] if item["id"] == args.source_id), None)
    if source is None or source.get("connector") not in CONNECTORS:
        print(f"未知或不可自动化来源：{args.source_id}", file=sys.stderr)
        return 2
    source_feed_defaults(source)
    cache = Path(os.getenv("XDG_CACHE_HOME", Path.home() / ".cache")) / "yjlcoder" / "remote-job-hunter"
    client = HttpClient(cache, ttl=args.cache_ttl)
    # urllib 的 socket timeout 不是整个响应的墙钟上限，所以给外层硬终止预留
    # 一个完整请求窗口。软预算到点后连接器会带着已抓到的岗位正常返回。
    client.set_deadline(time.monotonic() + max(1, args.source_timeout - DEFAULT_TIMEOUT - 5))
    try:
        jobs = CONNECTORS[source["connector"]](source, client, args.max_per_source)
    except Exception as exc:
        print(str(exc)[:1000], file=sys.stderr)
        return 1
    sys.stdout.write(json.dumps(jobs, ensure_ascii=False))
    return 0


def cmd_scan(args: argparse.Namespace) -> int:
    out = Path(args.output).resolve(); out.mkdir(parents=True, exist_ok=True)
    profile, taxonomy = prepare(args.profile)
    sources = load_json(REFERENCES / "sources.json")["sources"]
    selected = set(filter(None, (args.sources or "").split(",")))
    if selected: sources = [s for s in sources if s["id"] in selected]
    cache = Path(os.getenv("XDG_CACHE_HOME", Path.home() / ".cache")) / "yjlcoder" / "remote-job-hunter"
    client = HttpClient(cache, ttl=args.cache_ttl)
    print(f"[准备] 将检查 {len(sources)} 个来源；单来源最多 {args.source_timeout}s", flush=True)
    client.set_deadline(time.monotonic() + min(30, args.source_timeout))
    try:
        rates, rate_source = fetch_rates(client)
    finally:
        client.clear_deadline()
    print(f"[准备] 汇率数据：{rate_source}", flush=True)
    raw_jobs: list[dict[str, Any]] = []
    reports: list[dict[str, Any]] = []
    failed: set[str] = set()
    automated = [s for s in sources if s.get("connector") in CONNECTORS]
    def run(source: dict[str, Any]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        started = time.monotonic()
        try:
            process = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), "_source", "--source-id", source["id"],
                 "--max-per-source", str(args.max_per_source), "--cache-ttl", str(args.cache_ttl),
                 "--source-timeout", str(args.source_timeout)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                timeout=max(1, args.source_timeout), check=False,
            )
            if process.returncode != 0:
                jobs, status = [], "failed"
                error = (process.stderr.strip() or f"来源子进程退出码 {process.returncode}")[:500]
            else:
                jobs = json.loads(process.stdout)
                status = "ok" if jobs else "empty"
                error = "" if jobs else "公开通道返回 0 条；已生成搜索降级任务"
        except subprocess.TimeoutExpired:
            jobs, status, error = [], "timeout", f"达到 {args.source_timeout}s 硬超时，已终止该来源子进程"
        except Exception as exc:
            jobs, status, error = [], "failed", str(exc)[:500]
        return jobs, {"source_id": source["id"], "source": source["name"], "access": source["access"], "priority": source.get("priority", 0),
                      "status": status, "count": len(jobs), "elapsed_ms": round((time.monotonic() - started) * 1000), "error": error}
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, min(args.workers, 8))) as pool:
        futures = {pool.submit(run, source): source for source in automated}
        for completed, future in enumerate(concurrent.futures.as_completed(futures), 1):
            jobs, status = future.result(); raw_jobs.extend(jobs); reports.append(status)
            if status["status"] != "ok": failed.add(status["source_id"])
            print(
                f"[{completed}/{len(automated)}] {status['source']}: {status['status']} · "
                f"{status['count']} 条 · {status['elapsed_ms'] / 1000:.1f}s",
                flush=True,
            )
    for source in sources:
        if source not in automated:
            reports.append({"source_id": source["id"], "source": source["name"], "access": source["access"], "priority": source.get("priority", 0),
                            "status": "search_task" if source.get("automation") == "search_task" else source.get("automation"), "count": 0, "elapsed_ms": 0, "error": source.get("notes", "")})
    normalized = []
    for job in raw_jobs:
        try:
            item = normalize_job(job, profile, taxonomy, rates)
            if item.get("remote") and is_active(item, int(profile.get("freshness_days", 45))):
                normalized.append(item)
        except (ValueError, TypeError, KeyError): continue
    jobs = dedupe_jobs(normalized)
    accepted_by_source: dict[str, int] = {}
    for job in jobs:
        accepted_by_source[job["source_id"]] = accepted_by_source.get(job["source_id"], 0) + 1
    for status in reports:
        status["accepted_count"] = accepted_by_source.get(status["source_id"], 0)
        if status["status"] == "ok" and status["accepted_count"] == 0:
            status["status"] = "filtered"
            status["error"] = "抓到的记录均为现场、过期或无法形成有效岗位；已生成搜索降级任务"
            failed.add(status["source_id"])
    report_outputs(jobs, out, profile, reports, rate_source)
    tasks = build_search_tasks(sources, failed)
    atomic_write(out / "search_tasks.json", json.dumps(tasks, ensure_ascii=False, indent=2))
    coverage = {"generated_at": now_iso(), "sources_selected": len(sources), "automated_attempted": len(automated), "successful": sum(r["status"] == "ok" for r in reports),
                "failed_or_empty": len(failed), "raw_jobs": len(raw_jobs), "deduplicated_jobs": len(jobs), "search_tasks": len(tasks)}
    atomic_write(out / "coverage.json", json.dumps(coverage, ensure_ascii=False, indent=2))
    print(f"[完成] 报告已写入 {out}", flush=True)
    print(json.dumps(coverage, ensure_ascii=False, indent=2))
    return 0 if jobs or tasks else 2


def cmd_analyze(args: argparse.Namespace) -> int:
    out = Path(args.output).resolve(); out.mkdir(parents=True, exist_ok=True)
    profile, taxonomy = prepare(args.profile)
    source_index = {item["id"]: item for item in load_json(REFERENCES / "sources.json")["sources"]}
    client = HttpClient(Path(os.getenv("XDG_CACHE_HOME", Path.home() / ".cache")) / "yjlcoder" / "remote-job-hunter")
    rates, rate_source = fetch_rates(client)
    normalized = []
    for raw in read_jsonl(Path(args.input)):
        source_id = raw.get("source_id", "imported")
        source = dict(source_index.get(source_id, {}))
        source.update({"id": source_id, "name": raw.get("source", source.get("name", "imported")),
                       "homepage": raw.get("source_url", source.get("homepage", "")),
                       "priority": raw.get("source_priority", source.get("priority", 50))})
        base = blank_job(source, raw.get("url", "")); base.update(raw)
        try:
            item = normalize_job(base, profile, taxonomy, rates)
            if item.get("remote") and is_active(item, int(profile.get("freshness_days", 45))):
                normalized.append(item)
        except (ValueError, TypeError, KeyError): continue
    report_outputs(dedupe_jobs(normalized), out, profile, [], rate_source)
    print(f"已分析 {len(normalized)} 条，结果位于 {out}")
    return 0


def cmd_import(args: argparse.Namespace) -> int:
    existing = read_jsonl(Path(args.input)) if Path(args.input).exists() else []
    incoming = read_jsonl(Path(args.import_file))
    merged = existing + incoming
    write_jsonl(Path(args.input), merged)
    analyze_args = argparse.Namespace(input=args.input, profile=args.profile, output=args.output)
    return cmd_analyze(analyze_args)


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="搜索、归一化并排序全球远程岗位")
    sub = p.add_subparsers(dest="command", required=True)
    catalog = sub.add_parser("catalog", help="统计已审计站点"); catalog.set_defaults(func=cmd_catalog)
    scan = sub.add_parser("scan", help="抓取所有可自动化来源并生成报告")
    scan.add_argument("--profile", default=str(REFERENCES / "profile.json")); scan.add_argument("--output", default="remote-jobs")
    scan.add_argument("--sources", help="逗号分隔的 source id；空值表示全部"); scan.add_argument("--max-per-source", type=int, default=100)
    scan.add_argument("--workers", type=int, default=6); scan.add_argument("--cache-ttl", type=int, default=1800)
    scan.add_argument("--source-timeout", type=int, default=60, help="每个招聘来源的最长抓取秒数")
    scan.set_defaults(func=cmd_scan)
    analyze = sub.add_parser("analyze", help="只分析已有 JSONL")
    analyze.add_argument("--input", required=True); analyze.add_argument("--profile", default=str(REFERENCES / "profile.json")); analyze.add_argument("--output", default="remote-jobs"); analyze.set_defaults(func=cmd_analyze)
    imp = sub.add_parser("import", help="合并网页搜索补充 JSONL 后重新分析")
    imp.add_argument("--input", required=True); imp.add_argument("--import-file", required=True); imp.add_argument("--profile", default=str(REFERENCES / "profile.json")); imp.add_argument("--output", default="remote-jobs"); imp.set_defaults(func=cmd_import)
    worker = sub.add_parser("_source", help=argparse.SUPPRESS)
    worker.add_argument("--source-id", required=True); worker.add_argument("--max-per-source", type=int, default=100)
    worker.add_argument("--cache-ttl", type=int, default=1800); worker.add_argument("--source-timeout", type=int, default=60)
    worker.set_defaults(func=cmd_source_worker)
    return p


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
