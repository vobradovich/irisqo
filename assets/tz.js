// The time zone of the browser for the "New job" form: a `datetime-local` value has none, so
// it is sent along as the hidden `tz` field (an IANA name). Without it the server reads UTC.
(() => {
  let zone = "";
  try { zone = Intl.DateTimeFormat().resolvedOptions().timeZone || ""; } catch { /* UTC */ }
  if (!zone) return;
  // before htmx collects the values: the capture phase of the document goes first
  const fill = (event) => {
    const form = event.target instanceof Element ? event.target.closest("form") : null;
    form?.querySelectorAll('input[name="tz"]').forEach((input) => { input.value = zone; });
  };
  for (const type of ["input", "change", "submit"]) document.addEventListener(type, fill, true);
})();
