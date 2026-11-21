// Copy buttons: <button data-copy="value">, or <button data-copy-next> for the text of the
// element after the button's parent (a heading and the block under it). A click copies the
// value and shows a check for a moment. One listener on the document, so it serves the parts that htmx swaps in too.
(() => {
  const copy = async (text, button) => {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // not a secure context (http on a network address): the old way
      const field = document.createElement("textarea");
      field.value = text;
      field.setAttribute("readonly", "");
      field.style.position = "fixed";
      field.style.opacity = "0";
      document.body.appendChild(field);
      field.select();
      let ok = false;
      try { ok = document.execCommand("copy"); } catch { /* refused */ }
      field.remove();
      // the selection took the focus: give it back, or the keyboard lands at the top
      button.focus();
      return ok;
    }
  };

  document.addEventListener("click", async (event) => {
    const button = event.target.closest("button[data-copy], button[data-copy-next]");
    if (!button) return;
    // inside a summary or a link: only the copy
    event.preventDefault();
    event.stopPropagation();
    const text = button.hasAttribute("data-copy-next")
      ? (button.parentElement.nextElementSibling?.textContent ?? "")
      : button.dataset.copy;
    if (!(await copy(text, button))) return;
    button.classList.add("done");
    button.setAttribute("aria-label", "Copied");
    // the title keeps "Copy …": a second click in the meantime would save "Copied" instead
    setTimeout(() => {
      button.classList.remove("done");
      button.setAttribute("aria-label", button.title);
    }, 1400);
  });
})();
