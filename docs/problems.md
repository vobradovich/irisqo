# Problem types

Errors of the API are `application/problem+json` documents ([RFC 9457](https://www.rfc-editor.org/rfc/rfc9457)).
Every problem has `status` and `trace_id` (the trace of the request, when traced).

## invalid-parameters

`type`: `https://github.com/vobradovich/irisqo/blob/main/docs/problems.md#invalid-parameters`,
status `400`.

The request parameters are invalid: all of them are reported at once, in the `errors` extension.
Each error has:

- `parameter`: the field, as JSON names it (`delay`, `cron`, `callback.url`), however the job
  came: a `/to/` parameter `_delay` is the field `delay`, `_callback_url` is `callback.url`;
- `detail`: what is wrong, for a human.

```http
HTTP/1.1 400 Bad Request
Content-Type: application/problem+json

{
  "type": "https://github.com/vobradovich/irisqo/blob/main/docs/problems.md#invalid-parameters",
  "title": "Invalid parameters",
  "status": 400,
  "detail": "2 parameters are invalid.",
  "errors": [
    { "parameter": "delay", "detail": "must be a number of seconds" },
    { "parameter": "cron", "detail": "needs 5 or 6 fields, got 4: e.g. */15 * * * *" }
  ],
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

Fix the listed parameters and send the request again. Clients must ignore members they do not know:
an error of a JSON request body may carry a JSON `pointer` instead of `parameter` in the future.

## Other errors

No `type` (`about:blank`): the problem is what the HTTP status says, with its reason as `title`
(`Not Found`, `Internal Server Error`). A `404` says what is missing in `detail`
(`Job 42 does not exist.`). A server error carries no details of the implementation;
the server logs it with the `trace_id` of the response.
