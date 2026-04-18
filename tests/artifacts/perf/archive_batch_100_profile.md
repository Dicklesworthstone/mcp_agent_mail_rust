# Archive Batch 100 Profile

## Batch Comparison
- batch-1: p50=11732us, p95=12352us, p99=13517us, samples=40
- batch-10: p50=28817us, p95=31751us, p99=32664us, samples=25
- batch-100: p50=213468us, p95=238053us, p99=241759us, samples=12

## Top Span Categories
- `archive_batch.sample`: cumulative=2568827us, count=12, avg=214068us, max=241759us
- `archive_batch.write_message_batch`: cumulative=1751436us, count=12, avg=145953us, max=170140us
- `archive_batch.write_message_batch_bundle`: cumulative=1751391us, count=12, avg=145949us, max=170137us
- `archive_batch.flush_async_commits`: cumulative=816472us, count=12, avg=68039us, max=72357us
- `archive_batch.wbq_flush`: cumulative=601us, count=12, avg=50us, max=127us

## Scaling Law
- batch-10 p95 is 2.57x batch-1 and batch-100 p95 is 19.27x batch-1; amortized per-message cost at batch-100 is 0.193x batch-1.
