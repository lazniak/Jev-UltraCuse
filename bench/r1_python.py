"""R1 comparator: the Python reference (D:\\code\\JevUse, comtypes UIA + CacheRequest) scanning
the same window handles as `ultracuse bench-uia --hwnd`.

Usage (from the JevUse venv):
    D:\\code\\JevUse\\.venv\\Scripts\\python bench\\r1_python.py <hwnd> [<hwnd> ...]
Handles: (Get-Process notepad).MainWindowHandle
"""
import json
import statistics
import sys
import time

sys.path.insert(0, r"D:\code\JevUse")
from jevuse.perception import uia_fast, win32fast  # noqa: E402

win32fast.ensure_dpi_aware()
scanner = uia_fast.UiaScanner()
for hwnd in [int(a) for a in sys.argv[1:]]:
    totals = []
    last = None
    for _ in range(20):
        t0 = time.perf_counter()
        last = scanner.scan_hwnd(hwnd)
        totals.append((time.perf_counter() - t0) * 1000)
    totals.sort()
    print(json.dumps({
        "bench": "R1-uia-python", "hwnd": hwnd, "raw_count": last.raw_count, "kept": len(last.elements),
        "error": last.error,
        "total_ms": {"min": round(totals[0], 1), "p50": round(statistics.median(totals), 1),
                     "p95": round(totals[int(len(totals) * 0.95) - 1], 1), "max": round(totals[-1], 1)},
    }, ensure_ascii=False))
