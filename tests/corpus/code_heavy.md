# Code Review Notes

Please take a look at this pull request when you get a chance. Thank you so much in advance! I hope this helps set context: the team has been iterating on retry logic in the ingestion worker, and we think it is close to correct, but there is a subtle bug when the queue backs up under load.

Important: Please remember to double check the exponential backoff calculation before approving. The file `worker/ingest.py` computes `delay = base * (2 ** attempt)` and that can overflow for large `attempt` values on slow consumers.

## Python worker excerpt

```python
def process_batch(items, config):
    results = []
    for item in items:
        if item.status == "pending":
            if item.retries > config.max_retries:
                item.status = "failed"
                return None
            for attempt in range(item.retries, config.max_retries):
                try:
                    result = handle_item(item)
                except TransientError:
                    if attempt >= config.max_retries - 1:
                        return None
                    continue
                else:
                    results.append(result)
                    break
            else:
                return None
        else:
            if item.status == "done":
                continue
            else:
                return None
    return results


def handle_item(item):
    if item.payload is None:
        return None
    if not item.payload.get("id"):
        return None
    if item.payload.get("id") == "":
        return None
    return {"id": item.payload["id"], "ok": True}
```

## JS client excerpt

```js
function retryFetch(url, opts, attempt) {
  return fetch(url, opts).then((res) => {
    if (!res.ok) {
      if (attempt < 3) {
        return retryFetch(url, opts, attempt + 1);
      }
    }
    return res;
  }).catch((err) => {
    if (attempt < 3) {
      return retryFetch(url, opts, attempt + 1);
    }
    throw err;
  });
}

function parseResponse(res) {
  if (res.status === 200) {
    return res.json();
  }
}
```

Please also note that the `retryFetch` helper above is used at two different call sites, both of which need updating together if we change the backoff cap. For convenience (and so reviewers don't have to scroll back up), here it is again exactly as it appears at the second call site:

```js
function retryFetch(url, opts, attempt) {
  return fetch(url, opts).then((res) => {
    if (!res.ok) {
      if (attempt < 3) {
        return retryFetch(url, opts, attempt + 1);
      }
    }
    return res;
  }).catch((err) => {
    if (attempt < 3) {
      return retryFetch(url, opts, attempt + 1);
    }
    throw err;
  });
}

function parseResponse(res) {
  if (res.status === 200) {
    return res.json();
  }
}
```

## Proposed diff

```diff
--- a/worker/ingest.py
+++ b/worker/ingest.py
@@ -12,7 +12,10 @@ def compute_delay(attempt, base=1.0):
-    return base * (2 ** attempt)
+    return min(base * (2 ** attempt), 300.0)
 
 
 def process_batch(items, config):
     results = []
     for item in items:
-        if item.retries > config.max_retries:
+        if item.retries >= config.max_retries:
             item.status = "failed"
             return None
```

## Notes

Thank you so much for reviewing this, please be careful with the `delay` cap since some downstream consumers assume unbounded backoff. Also double check the `handle_item` function — it has some  double  spaces  in a couple of comments that got left over from a find/replace, and the inline code span for `config.max_retries  ` had trailing whitespace baked in during a previous edit.

Data collected from the last three staging runs:

| Run | Attempts | Result |
|-----|----------|--------|
| 1   | 2        | ok     |
| 2   | 5        | failed |
| 3   | 1        | ok     |

- retries: 3
- retries: 3
- backoff base: 1.0
- backoff base: 1.0
- max delay: 300
- queue: ingestion
- consumers: 4
- Please be careful
- Note: this is important
