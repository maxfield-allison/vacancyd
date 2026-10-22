// Preserve Zensical's checkbox-label toggles while exposing keyboard controls.
for (const label of document.querySelectorAll(
  '.md-header label[for="__search"], .md-header label[for="__drawer"]',
)) {
  label.setAttribute("role", "button");
  label.tabIndex = 0;
  if (label.htmlFor === "__search") {
    label.setAttribute("aria-haspopup", "dialog");
  }
  label.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      if (!event.repeat) label.click();
    }
  });
}
