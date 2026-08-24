import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "job_hunter.py"
HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("job_hunter", SCRIPT)
job_hunter = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(job_hunter)


class JobHunterExistingTests(unittest.TestCase):
    def setUp(self):
        self.source = {"id": "fixture", "name": "Fixture", "homepage": "https://example.test", "priority": 100}
        self.profile = job_hunter.load_json(HERE.parent / "references" / "profile.json")
        self.taxonomy = job_hunter.load_json(HERE.parent / "references" / "tech-taxonomy.json")["technologies"]

    def test_jsonld_end_to_end(self):
        raw = (HERE / "fixtures" / "job_page.html").read_bytes()
        jobs, _ = job_hunter.parse_jsonld_page(raw, self.source, "https://example.test/list")
        self.assertEqual(len(jobs), 1)
        job = job_hunter.normalize_job(jobs[0], self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertEqual(job["company"], "Fixture Labs")
        self.assertEqual(job["eligibility"], "eligible")
        self.assertEqual(job["experience_min_years"], 4)
        self.assertIn("Rust", job["skills"])
        self.assertIn("Solana", job["skills"])
        self.assertAlmostEqual(job["salary_usd_annual_min"], 93600, delta=10)
        self.assertIn("15:00", job["beijing_hours"])

    def test_hourly_salary_is_annualized(self):
        job = job_hunter.blank_job(self.source, "https://example.test/jobs/go")
        job.update({"title": "Go contractor", "description": "Remote worldwide. USD 50-75/hour", "remote": True})
        normalized = job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertEqual(normalized["salary_usd_annual_min"], 104000)
        self.assertEqual(normalized["salary_usd_annual_max"], 156000)

    def test_us_only_is_not_recommended(self):
        job = job_hunter.blank_job(self.source, "https://example.test/jobs/cpp")
        job.update({"title": "C++ Systems Engineer", "description": "Remote US only. Must be based in the United States.", "remote": True})
        normalized = job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertEqual(normalized["eligibility"], "ineligible")
        self.assertLessEqual(normalized["score"], 25)

    def test_company_serves_worldwide_does_not_make_onsite_job_remote(self):
        job = job_hunter.blank_job(self.source, "https://example.test/jobs/onsite")
        job.update({"title": "Growth Manager", "location": "Chicago, IL, USA", "description": "Our company serves customers worldwide. This is an in-office role.", "remote": True})
        normalized = job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertFalse(normalized["remote"])
        self.assertEqual(normalized["eligibility"], "ineligible")

    def test_years_are_not_parsed_as_salary(self):
        self.assertEqual(job_hunter.parse_salary_text("Minimum 5 years experience and 40 hours per week"), {})

    def test_funding_and_trading_volume_are_not_salary(self):
        self.assertEqual(job_hunter.parse_salary_text("We raised $18M at Series A and process $285B in trading volume."), {})

    def test_europe_remote_is_ineligible_from_china(self):
        job = job_hunter.blank_job(self.source, "https://example.test/jobs/eu")
        job.update({"title": "Go Engineer", "location": "Remote Europe", "description": "This role is fully remote within Europe."})
        normalized = job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertEqual(normalized["eligibility"], "ineligible")

    def test_duplicate_prefers_priority_and_content(self):
        first = job_hunter.blank_job(self.source, "https://board-a.test/jobs/42")
        first.update({"title": "Rust Engineer", "company": "Same Co", "description": "short", "source_priority": 70})
        second = job_hunter.blank_job(self.source, "https://board-b.test/jobs/42")
        second.update({"title": "Rust Engineer", "company": "Same Co", "description": "long " * 200, "source_priority": 100})
        result = job_hunter.dedupe_jobs([first, second])
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["source_priority"], 100)

    def test_catalog_contains_all_requested_sites(self):
        sources = job_hunter.load_json(HERE.parent / "references" / "sources.json")["sources"]
        ids = {source["id"] for source in sources}
        expected = {"web3career", "cryptojobslist", "cryptocurrencyjobs", "remote3", "web3jobsio", "blockjobs", "web3vacancy", "solana_jobs", "rustjobs", "jobsinrust", "golangcafe", "golangprojects", "remoteok", "weworkremotely", "wellfound", "himalayas", "arc", "workingnomads", "upwork", "toptal"}
        self.assertTrue(expected <= ids)

    def test_alternative_job_catalog_does_not_pollute_skills(self):
        job = job_hunter.blank_job(self.source, "https://example.test/jobs/designer")
        job.update({"title": "Senior Graphic Designer", "description":
                    "Requirements: Adobe Illustrator and strong design craft. NOT YOUR TECH STACK? "
                    "Other projects need Rust, C++, Golang, Kubernetes, React and Python.",
                    "location": "Remote", "remote": True})
        normalized = job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())
        self.assertNotIn("Rust", normalized["skills"])
        self.assertNotIn("C++", normalized["skills"])
        self.assertNotIn("Go", normalized["skills"])


class SalaryQualityTests(unittest.TestCase):
    def test_salary_parser_keeps_thousands(self):
        parsed = job_hunter.parse_salary_text("Salary range: $100,000 - $200,000 per year")
        self.assertEqual(parsed["salary_min"], 100_000)
        self.assertEqual(parsed["salary_max"], 200_000)
        self.assertEqual(parsed["salary_period"], "year")

    def test_funding_is_not_salary(self):
        parsed = job_hunter.parse_salary_text("We raised $220M in Series B funding and are hiring engineers")
        self.assertEqual(parsed, {})

    def test_impossible_structured_salary_is_quarantined(self):
        source = {"id": "test", "name": "Test", "homepage": "https://example.com"}
        job = job_hunter.blank_job(source, "https://example.com/job")
        job.update({"salary_min": 2.0, "salary_max": 0.0, "salary_currency": "USD", "salary_period": "year"})
        job_hunter.annualize_salary(job, job_hunter.default_rates())
        self.assertIsNone(job["salary_usd_annual_min"])
        self.assertEqual(job["salary_confidence"], "rejected")
        self.assertTrue(job["salary_rejected_reason"])

    def test_missing_salary_period_is_not_assumed_annual(self):
        source = {"id": "test", "name": "Test", "homepage": "https://example.com"}
        job = job_hunter.blank_job(source, "https://example.com/job")
        job.update({"salary_min": 31.0, "salary_max": 31.0, "salary_currency": "USD",
                    "salary_original": "$31"})
        job_hunter.annualize_salary(job, job_hunter.default_rates())
        self.assertIsNone(job["salary_usd_annual_min"])
        self.assertFalse(job["salary_rejected_reason"])
        self.assertIn("周期未公开", job["warnings"][-1])

    def test_learning_budget_is_not_salary(self):
        text = "Personal learning and development budget of USD 2000 per year. Annual compensation review."
        self.assertEqual(job_hunter.parse_salary_text(text), {})

    def test_previously_parsed_learning_budget_is_quarantined(self):
        source = {"id": "test", "name": "Test", "homepage": "https://example.com"}
        job = job_hunter.blank_job(source, "https://example.com/job")
        job.update({"salary_min": 2000.0, "salary_max": 2000.0, "salary_currency": "USD",
                    "salary_period": "year", "salary_kind": "explicit_text",
                    "salary_evidence": "Personal learning and development budget of USD 2000 per year. Annual compensation review."})
        job_hunter.annualize_salary(job, job_hunter.default_rates())
        self.assertIsNone(job["salary_usd_annual_min"])
        self.assertIn("福利预算", job["salary_rejected_reason"])

    def test_aggregator_estimate_is_replaced_by_explicit_description(self):
        source = {"id": "blockjobs", "name": "BlockJobs", "homepage": "https://blockjobs.careers/",
                  "salary_trust": "estimate", "eligibility_trust": "unverified_aggregator"}
        job = job_hunter.blank_job(source, "https://employer.example/job")
        job.update({
            "title": "Graduate Backend Engineer", "company": "Example", "description":
            "Starting Salary: £50,000 Location: London. Join the backend team.",
            "location": "Worldwide", "remote": True, "salary_min": 100_000,
            "salary_max": 200_000, "salary_currency": "USD", "salary_period": "year",
            "salary_kind": "explicit_structured", "salary_original": "USD 100000-200000",
        })
        profile = {"languages": [{"name": "Rust"}], "other_skills": [], "accept_night_shift": True,
                   "freshness_days": 45}
        taxonomy = {"Backend": ["backend"], "Rust": ["rust"]}
        normalized = job_hunter.normalize_job(job, profile, taxonomy, job_hunter.default_rates())
        self.assertEqual(normalized["salary_kind"], "explicit_text")
        self.assertEqual(normalized["salary_min"], 50_000)
        self.assertEqual(normalized["salary_currency"], "GBP")
        self.assertEqual(normalized["location"], "London")
        self.assertEqual(normalized["eligibility"], "unknown")


class LearningRoadmapTests(unittest.TestCase):
    def test_roadmap_contains_evidence_and_acceptance_steps(self):
        profile = {"profile_name": "learner", "languages": [{"name": "Rust", "years": None}],
                   "other_skills": ["Linux"]}
        jobs = [{
            "title": "Rust Protocol Engineer", "eligibility": "eligible", "score": 70,
            "skills": ["Rust", "Web3", "Kubernetes", "System Design"],
            "skill_requirements": {"Rust": "required", "Web3": "required", "Kubernetes": "preferred", "System Design": "required"},
        }]
        roadmap = job_hunter.build_learning_roadmap(jobs, profile)
        self.assertIn("12 周执行表", roadmap)
        self.assertIn("明确必需", roadmap)
        self.assertIn("通过标准", roadmap)
        self.assertIn("Rust 异步 RPC/链上索引器", roadmap)
        self.assertIn("画像里 Rust 的年限/熟练度仍是未知", roadmap)

    def test_mentioned_languages_do_not_score_like_required_language(self):
        profile = {"languages": [{"name": "Rust"}, {"name": "C++"}, {"name": "Go"}],
                   "other_skills": ["Linux"], "accept_night_shift": True, "freshness_days": 45}
        mentioned = {"title": "Engineering Manager", "description": "", "skills": ["Rust", "C++", "Go", "Linux"],
                     "skill_requirements": {"Rust": "mentioned", "C++": "mentioned", "Go": "mentioned", "Linux": "mentioned"},
                     "eligibility": "eligible", "timezone_original": "未公开", "published_at": None,
                     "source_priority": 50, "salary_kind": "unknown", "salary_usd_annual_max": None}
        required = dict(mentioned)
        required.update({"title": "Rust Protocol Engineer", "skills": ["Rust", "Linux"],
                         "skill_requirements": {"Rust": "required", "Linux": "required"}})
        job_hunter.score_job(mentioned, profile, job_hunter.profile_skill_set(profile))
        job_hunter.score_job(required, profile, job_hunter.profile_skill_set(profile))
        self.assertGreater(required["score_breakdown"]["skill"], mentioned["score_breakdown"]["skill"])


class EarlyCareerPriorityTests(unittest.TestCase):
    def setUp(self):
        self.source = {"id": "fixture", "name": "Fixture", "homepage": "https://example.test", "priority": 100}
        self.profile = job_hunter.load_json(HERE.parent / "references" / "profile.json")
        self.taxonomy = job_hunter.load_json(HERE.parent / "references" / "tech-taxonomy.json")["technologies"]

    def normalized(self, title, description, path):
        job = job_hunter.blank_job(self.source, f"https://example.test/jobs/{path}")
        job.update({"title": title, "description": description, "location": "Worldwide", "remote": True})
        return job_hunter.normalize_job(job, self.profile, self.taxonomy, job_hunter.default_rates())

    def test_supported_graduate_job_outranks_senior_lead(self):
        junior = self.normalized(
            "Graduate Rust Engineer",
            "This role is fully remote worldwide. Requirements: 0-2 years of experience with Rust and Linux. "
            "You receive a mentor and structured training program. Benefits include medical insurance and paid time off.",
            "graduate",
        )
        senior = self.normalized(
            "Rust Engineering Lead",
            "This role is fully remote worldwide. Requirements: 6+ years of experience with Rust and Linux.",
            "lead",
        )
        self.assertEqual(junior["priority_tier"], "S 培养型低门槛")
        self.assertEqual(junior["career_stage"], "graduate")
        self.assertIn("明确导师/mentorship", junior["growth_signals"])
        self.assertIn("医疗保险", junior["benefit_signals"])
        self.assertGreater(junior["score"], senior["score"])
        self.assertEqual(senior["priority_tier"], "C 高门槛/不符合")

    def test_learning_budget_is_benefit_but_not_salary(self):
        job = self.normalized(
            "Junior Go Engineer",
            "Remote worldwide. Entry-level role with a mentor. We provide a $2,000 learning budget per year and health insurance.",
            "learning-budget",
        )
        self.assertIn("学习预算", job["benefit_signals"])
        self.assertIsNone(job["salary_usd_annual_min"])

    def test_application_queue_stops_at_profile_gate_by_default(self):
        job = self.normalized(
            "Rust Intern",
            "Remote worldwide internship. Mentorship and paid training. Health insurance and paid time off.",
            "intern",
        )
        policy = job_hunter.load_application_policy()
        queue = job_hunter.build_application_queue([job], policy)
        self.assertEqual(len(queue), 1)
        self.assertEqual(queue[0]["state"], "blocked_profile")
        self.assertIn("resume_path", queue[0]["missing_requirements"])

    def test_complete_dry_run_policy_never_enters_live_queue(self):
        job = self.normalized(
            "Junior C++ Engineer",
            "Remote worldwide. Junior role with structured onboarding and coaching. Dental insurance and paid holidays.",
            "cpp-junior",
        )
        policy = job_hunter.load_application_policy()
        with tempfile.TemporaryDirectory() as temp_dir:
            resume = Path(temp_dir) / "resume.pdf"
            resume.write_bytes(b"fixture")
            policy.update({"enabled": True, "dry_run": True, "resume_path": str(resume)})
            policy["contact"] = {"full_name": "Test User", "email": "test@example.test", "country": "CN"}
            queue = job_hunter.build_application_queue([job], policy)
        self.assertEqual(queue[0]["state"], "dry_run_ready")
        self.assertEqual(queue[0]["adapter"], "generic_browser")


if __name__ == "__main__":
    unittest.main()
