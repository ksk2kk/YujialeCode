import importlib.util
import pathlib
import unittest


HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE.parent / "scripts" / "job_hunter.py"
SPEC = importlib.util.spec_from_file_location("job_hunter", SCRIPT)
job_hunter = importlib.util.module_from_spec(SPEC)
assert SPEC.loader
SPEC.loader.exec_module(job_hunter)


class JobHunterTests(unittest.TestCase):
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
        parsed = job_hunter.parse_salary_text("Minimum 5 years experience and 40 hours per week")
        self.assertEqual(parsed, {})

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


if __name__ == "__main__":
    unittest.main()
