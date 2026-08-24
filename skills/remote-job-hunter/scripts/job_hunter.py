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
import json
import math
import os
from pathlib import Path
import re
import ssl
import sys
import time
from typing import Any, Iterable
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from zoneinfo import ZoneInfo


ROOT = Path(__file__).resolve().parent.parent
REFERENCES = ROOT / "references"
USER_AGENT = "YJLCoder-RemoteJobHunter/1.0 (+https://github.com/ksk2kk/YujialeCode)"
DEFAULT_TIMEOUT = 25
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
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=UTC)
        return max(0.0, (dt.datetime.now(UTC) - parsed.astimezone(UTC)).total_seconds() / 86400)
    except ValueError:
        return None


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
        for attempt in range(3):
            try:
                self.host_last[host] = time.monotonic()
                with urllib.request.urlopen(request, timeout=self.timeout, context=self.context) as response:
                    data = response.read(12_000_000)
                    if not data:
                        raise RuntimeError("空响应")
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
    salary = obj.get("baseSalary") or obj.get("estimatedSalary")
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
        job["salary_kind"] = "explicit"
    return job


def number(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if value is None:
        return None
    raw = str(value).replace(",", "").strip().lower()
    match = re.search(r"-?\d+(?:\.\d+)?", raw)
    if not match:
        return None
    n = float(match.group())
    if "k" in raw[match.end():match.end() + 2]:
        n *= 1000
    return n


def parse_salary_text(text: str) -> dict[str, Any]:
    compact = text.replace(",", " ")
    pattern = re.compile(
        r"(?i)(?P<currency>USD|EUR|GBP|CAD|AUD|CNY|RMB|CHF|SEK|NOK|DKK|PLN|INR|\$|€|£|¥)?\s*"
        r"(?P<low>\d+(?:\.\d+)?\s*[km]?)\s*(?:-|–|—|to)?\s*"
        r"(?P<high>\d+(?:\.\d+)?\s*[km]?)?\s*"
        r"(?P<currency2>USD|EUR|GBP|CAD|AUD|CNY|RMB|CHF|SEK|NOK|DKK|PLN|INR)?\s*"
        r"(?:/|per\s+)?(?P<period>year|annual|annum|yr|month|monthly|week|weekly|day|daily|hour|hourly|hr)?"
    )
    candidates = []
    for m in pattern.finditer(compact):
        currency_raw = m.group("currency") or m.group("currency2")
        period = (m.group("period") or "").lower()
        low = number(m.group("low"))
        high = number(m.group("high")) if m.group("high") else low
        # 年限、工时、版本号里也经常出现 “5 years/20 hour”。没有明确货币时
        # 宁可留空，也不能把它们包装成薪资。
        if not currency_raw:
            continue
        context = compact[max(0, m.start() - 80):m.end() + 80].lower()
        if not period and not re.search(r"salary|compensation|pay\b|base\s+pay|wage|rate\b|budget|earn", context):
            # “公司融资 $18M”“平台成交 $285B”不是求职者收入。
            continue
        currency = CURRENCY_SYMBOLS.get(currency_raw, currency_raw.upper() if currency_raw else None)
        if currency == "RMB":
            currency = "CNY"
        if low is not None and low >= 1:
            candidates.append((m.group(0).strip(), low, high, currency, period or None))
    if not candidates:
        return {}
    chosen = max(candidates, key=lambda x: (bool(x[3]), bool(x[4]), x[2] or 0))
    return {"salary_original": chosen[0], "salary_min": chosen[1], "salary_max": chosen[2], "salary_currency": chosen[3], "salary_period": chosen[4], "salary_kind": "explicit"}


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


def annualize_salary(job: dict[str, Any], rates: dict[str, float]) -> None:
    if job.get("salary_min") is None:
        parsed = parse_salary_text(" ".join((job.get("salary_original", ""), job.get("description", "")[:20000])))
        for key, value in parsed.items():
            if job.get(key) in (None, "", "未公开", "unknown"):
                job[key] = value
    low, high = job.get("salary_min"), job.get("salary_max")
    cur = (job.get("salary_currency") or "").upper()
    if low is None or cur not in rates:
        return
    period = (job.get("salary_period") or "year").lower()
    factor = next((v for k, v in PERIOD_FACTORS.items() if k in period), 1.0)
    usd_low = float(low) * factor * rates[cur]
    usd_high = float(high if high is not None else low) * factor * rates[cur]
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


def normalize_job(job: dict[str, Any], profile: dict[str, Any], taxonomy: dict[str, list[str]], rates: dict[str, float]) -> dict[str, Any]:
    job["title"] = clean_html(job.get("title"))[:500]
    job["company"] = clean_html(job.get("company"))[:300] or "未公开"
    job["description"] = clean_html(job.get("description"))[:120000]
    job["location"] = clean_html(job.get("location"))[:500] or "未公开"
    job["url"] = canonical_url(as_text(job.get("url") or job.get("apply_url")))
    if not job["title"] or not job["url"]:
        raise ValueError("岗位缺少 title 或 url")
    infer_remote(job)
    exp_min, exp_max = parse_experience(job["description"])
    job["experience_min_years"] = job.get("experience_min_years") if job.get("experience_min_years") is not None else exp_min
    job["experience_max_years"] = job.get("experience_max_years") if job.get("experience_max_years") is not None else exp_max
    hours, duration = parse_hours(job["description"])
    job["weekly_hours"] = job.get("weekly_hours") if job.get("weekly_hours") is not None else hours
    if job.get("contract_duration") in (None, "", "未公开"):
        job["contract_duration"] = duration
    infer_schedule(job)
    job["skills"] = extract_skills(" ".join((job["title"], job["description"])), taxonomy)
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
        skill_score = 20 * len(skills & profile_skills) / max(1, len(skills)) + 15 * min(1, len(skills & primary))
    elif any(x.lower() in (job["title"] + " " + job["description"]).lower() for x in ("rust", "golang", "c++")):
        skill_score = 25
    eligibility_score = {"eligible": 20, "unknown": 7, "ineligible": 0}[job["eligibility"]]
    salary = job.get("salary_usd_annual_max")
    income_score = 6 if salary is None else min(18, 5 + 13 * math.log10(max(1000, salary) / 1000) / 2.5)
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
    if re.search(r"\b(staff|principal|lead|architect|director|head|cto)\b", text): difficulty += 3
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
                              "source": round(source_bonus, 1)}


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
    patterns = [re.compile(p) for p in source.get("job_path_patterns", [r"/jobs?/"])]
    for url in source.get("list_urls", [source["homepage"]]):
        raw = client.get(url)
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
    seen_urls = {j.get("url") for j in jobs}
    for url, title in list(detail_links.items())[:max_items]:
        if url in seen_urls:
            continue
        try:
            parsed = generic_page_job(client.get(url), source, url, title)
            if parsed:
                jobs.append(parsed)
        except Exception:
            continue
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
        job.update(parse_salary_text(as_text(obj.get("salary")) + " " + job["description"]))
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
        job.update(parse_salary_text(job["salary_original"]))
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
                    "salary_period": "year"})
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
            job.update(parse_salary_text(job["salary_original"]))
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
    if job.get("salary_usd_annual_min") is None: return "未公开"
    return f"USD {job['salary_usd_annual_min']:,.0f}–{job['salary_usd_annual_max']:,.0f}/年；约 RMB {job['salary_cny_annual_min']:,.0f}–{job['salary_cny_annual_max']:,.0f}/年"


def report_outputs(jobs: list[dict[str, Any]], out: Path, profile: dict[str, Any], source_reports: list[dict[str, Any]], rate_source: str) -> None:
    jobs.sort(key=lambda j: (j.get("eligibility") == "eligible", j.get("score", 0), j.get("salary_usd_annual_max") or 0), reverse=True)
    write_jsonl(out / "jobs.jsonl", jobs)
    columns = ["score", "eligibility", "title", "company", "source", "location", "remote_scope", "experience_min_years",
               "weekly_hours", "contract_duration", "timezone_original", "beijing_hours", "salary_original",
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
    recommended = [job for job in jobs if job.get("eligibility") == "eligible"]
    pending = [job for job in jobs if job.get("eligibility") == "unknown"]
    lines = ["# 全球远程岗位报告", "", f"生成时间：{now_iso()} ；画像：{profile.get('profile_name', 'default')}；汇率：{rate_source}。",
             "", "> 未写明的薪资、经验、工时、期限和时区均显示为“未公开”。北京时间只在能识别时区时推算。远程但资格不明的岗位需要人工确认。", "",
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
    lines += ["", "## 最常见技术栈", "", "|技术|出现岗位数|个人画像未覆盖|", "|---|---:|---:|"]
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


def cmd_scan(args: argparse.Namespace) -> int:
    out = Path(args.output).resolve(); out.mkdir(parents=True, exist_ok=True)
    profile, taxonomy = prepare(args.profile)
    sources = load_json(REFERENCES / "sources.json")["sources"]
    selected = set(filter(None, (args.sources or "").split(",")))
    if selected: sources = [s for s in sources if s["id"] in selected]
    cache = Path(os.getenv("XDG_CACHE_HOME", Path.home() / ".cache")) / "yjlcoder" / "remote-job-hunter"
    client = HttpClient(cache, ttl=args.cache_ttl)
    rates, rate_source = fetch_rates(client)
    raw_jobs: list[dict[str, Any]] = []
    reports: list[dict[str, Any]] = []
    failed: set[str] = set()
    automated = [s for s in sources if s.get("connector") in CONNECTORS]
    def run(source: dict[str, Any]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        source_feed_defaults(source)
        started = time.monotonic()
        try:
            jobs = CONNECTORS[source["connector"]](source, client, args.max_per_source)
            status = "ok" if jobs else "empty"
            error = "" if jobs else "公开通道返回 0 条；已生成搜索降级任务"
        except Exception as exc:
            jobs, status, error = [], "failed", str(exc)[:500]
        return jobs, {"source_id": source["id"], "source": source["name"], "access": source["access"], "priority": source.get("priority", 0),
                      "status": status, "count": len(jobs), "elapsed_ms": round((time.monotonic() - started) * 1000), "error": error}
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, min(args.workers, 8))) as pool:
        futures = {pool.submit(run, source): source for source in automated}
        for future in concurrent.futures.as_completed(futures):
            jobs, status = future.result(); raw_jobs.extend(jobs); reports.append(status)
            if status["status"] != "ok": failed.add(status["source_id"])
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
    print(json.dumps(coverage, ensure_ascii=False, indent=2))
    return 0 if jobs or tasks else 2


def cmd_analyze(args: argparse.Namespace) -> int:
    out = Path(args.output).resolve(); out.mkdir(parents=True, exist_ok=True)
    profile, taxonomy = prepare(args.profile)
    client = HttpClient(Path(os.getenv("XDG_CACHE_HOME", Path.home() / ".cache")) / "yjlcoder" / "remote-job-hunter")
    rates, rate_source = fetch_rates(client)
    normalized = []
    for raw in read_jsonl(Path(args.input)):
        source = {"id": raw.get("source_id", "imported"), "name": raw.get("source", "imported"), "homepage": raw.get("source_url", ""), "priority": raw.get("source_priority", 50)}
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
    scan.add_argument("--workers", type=int, default=6); scan.add_argument("--cache-ttl", type=int, default=1800); scan.set_defaults(func=cmd_scan)
    analyze = sub.add_parser("analyze", help="只分析已有 JSONL")
    analyze.add_argument("--input", required=True); analyze.add_argument("--profile", default=str(REFERENCES / "profile.json")); analyze.add_argument("--output", default="remote-jobs"); analyze.set_defaults(func=cmd_analyze)
    imp = sub.add_parser("import", help="合并网页搜索补充 JSONL 后重新分析")
    imp.add_argument("--input", required=True); imp.add_argument("--import-file", required=True); imp.add_argument("--profile", default=str(REFERENCES / "profile.json")); imp.add_argument("--output", default="remote-jobs"); imp.set_defaults(func=cmd_import)
    return p


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
