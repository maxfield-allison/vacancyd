const choices = [...document.querySelectorAll("[data-presence]")];
const sign = document.querySelector(".vacancy-sign");
for (const button of choices) {
  button.addEventListener("click", () => {
    const away = button.dataset.presence === "away";
    for (const choice of choices)
      choice.setAttribute("aria-pressed", String(choice === button));
    sign.dataset.vacancy = String(away);
    sign.setAttribute("aria-label", away ? "Vacancy" : "No vacancy");
    document.querySelector("#demo-title").textContent = away
      ? "Room for a temporary resident."
      : "Owner policy pauses new work.";
    document.querySelector("#demo-description").textContent = away
      ? "This example route can accept work while its owner is away and the resource checks pass."
      : "This example route refuses new borrowed requests. Already admitted work follows its configured reclaim policy.";
    document.querySelector("#route-result").textContent = away
      ? "Inference server"
      : "New request declined";
  });
}

// Content is visible by default, including without JavaScript or when reduced motion is preferred.
if (
  !window.matchMedia("(prefers-reduced-motion: reduce)").matches &&
  "IntersectionObserver" in window
) {
  document.documentElement.classList.add("js-motion");
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries)
        if (entry.isIntersecting) {
          entry.target.classList.add("is-visible");
          observer.unobserve(entry.target);
        }
    },
    { threshold: 0.08 },
  );
  for (const element of document.querySelectorAll(".reveal"))
    observer.observe(element);
}

async function showRelease() {
  try {
    const response = await fetch("/release.json");
    if (!response.ok) throw new Error("Release information is unavailable");
    const release = await response.json();
    if (!release.available) return;
    const downloads = document.querySelector("#release-downloads");
    for (const asset of release.downloads) {
      const url = new URL(asset.url);
      if (
        url.origin !== "https://github.com" ||
        !url.pathname.startsWith(
          "/maxfield-allison/vacancyd/releases/download/",
        )
      )
        throw new Error("Unexpected release destination");
      const link = document.createElement("a");
      link.className = "button";
      link.href = url.href;
      link.textContent = asset.label;
      link.dataset.umamiEvent = asset.event;
      downloads.append(link);
    }
    document.querySelector("#release-status").textContent =
      `${release.version} is available. Verify downloads with the signed checksums in the release.`;
    downloads.hidden = false;
  } catch {
    // Source and documentation remain usable; an unverified download is never substituted.
    document.querySelector("#release-status").textContent =
      "Source and guides are available. Check GitHub for the latest verified release.";
  }
}
showRelease();
