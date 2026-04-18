# Archive Batch 100 Profile

## Batch Comparison
- batch-1: p50=11916us, p95=12701us, p99=14964us, samples=40
- batch-10: p50=33073us, p95=38239us, p99=38879us, samples=25
- batch-100: p50=220397us, p95=264709us, p99=265006us, samples=12

## Top Span Categories
- `archive_batch.sample`: cumulative=2649091us, count=12, avg=220757us, max=265007us
- `archive_batch.write_message_loop`: cumulative=2039448us, count=12, avg=169954us, max=178158us
- `archive_batch.write_message_bundle`: cumulative=2035491us, count=1200, avg=1696us, max=12151us
- `archive_batch.flush_async_commits`: cumulative=608879us, count=12, avg=50739us, max=112385us
- `archive_batch.wbq_flush`: cumulative=420us, count=12, avg=35us, max=49us

## Scaling Law
- batch-10 p95 is 3.01x batch-1 and batch-100 p95 is 20.84x batch-1; amortized per-message cost at batch-100 is 0.208x batch-1.
