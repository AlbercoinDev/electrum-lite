#!/usr/bin/env python3
"""Offline classification tests: never count a stalled Core as a speed win."""
import unittest
from cosync_smoke import summarize


def sample(t, core, lite):
    return {"elapsed": t, "core_before": core, "core_after": core,
            "lite_height": lite, "lag_lower": max(0, core-lite),
            "lag_upper": max(0, core-lite), "last_error": None}


class CosyncTests(unittest.TestCase):
    def test_true_sustained_reduction(self):
        samples = [sample(t, 10000 + 10*t, 20*t) for t in range(0, 181, 2)]
        self.assertEqual(summarize(samples, 120)["result"], "BOUNDED_COSYNC_LAG_REDUCTION")

    def test_growing_lag_is_not_success(self):
        samples = [sample(t, 10000 + 20*t, 10*t) for t in range(0, 181, 2)]
        self.assertEqual(summarize(samples, 120)["result"], "NOT_DEMONSTRATED")

    def test_catching_stopped_core_is_not_success(self):
        samples = [sample(t, 10000, 20*t) for t in range(0, 181, 2)]
        self.assertEqual(summarize(samples, 120)["result"], "NOT_DEMONSTRATED")

    def test_only_catching_during_stalls_is_not_success(self):
        samples = [sample(t, 10000 + (t//20)*200, (t//20)*100 + (t%20)*10)
                   for t in range(0, 181, 2)]
        self.assertEqual(summarize(samples, 120)["result"], "NOT_DEMONSTRATED")

    def test_faulted_samples_cannot_qualify(self):
        samples = [sample(t, 10000 + 10*t, 20*t) | {"last_error": "disk error"}
                   for t in range(0, 181, 2)]
        self.assertEqual(summarize(samples, 120)["result"], "NOT_DEMONSTRATED")


if __name__ == "__main__":
    unittest.main()
