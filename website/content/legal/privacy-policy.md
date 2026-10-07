---
title: "Privacy Policy"
description: "What this site and the pixel binary do with your data: the site uses no cookies, and four providers see your IP address when you visit. The binary sends no telemetry beyond an optional release check."
---

<!-- This page restates what the templates load: the Google Fonts request in layouts/partials/head.html, the GitHub API call, the Cloudflare beacon and the sessionStorage keys in layouts/partials/footer.html, head.html and splash.html, and the network access SECURITY.md lists. Change it when they change. -->

## Who is responsible

This site, pixel-cli.dev, is published by Navid Emad, the main contributor to [Pixel](https://github.com/Pixel-CLI/pixel). Livio Gama created this open-source project and is a major contributor too ([About](/about/)). For any question about this policy or your data, open a [discussion on GitHub](https://github.com/Pixel-CLI/pixel/discussions). To send a request privately, use a [private advisory](https://github.com/Pixel-CLI/pixel/security/advisories/new).

## What this site collects

The site has no accounts, no forms, no newsletter and no comments, and it sets no cookies. The publisher receives no personal data from it.

When your browser loads a page, it contacts the four providers below. Each one receives your IP address, your browser's user agent and the page you requested, which is how any web request works. Each provider is an independent controller of its own logs:

| Provider | Why the browser contacts it | Its policy |
| --- | --- | --- |
| GitHub Pages (GitHub, Inc.) | hosts the site and serves every page | [GitHub General Privacy Statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement) |
| Google Fonts (Google LLC) | serves the three typefaces the pages use | [Google Fonts privacy FAQ](https://developers.google.com/fonts/faq/privacy) |
| GitHub API (GitHub, Inc.) | returns the repository's star count shown in the navigation bar | [GitHub General Privacy Statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement) |
| Cloudflare Web Analytics (Cloudflare, Inc.) | counts visits | [Cloudflare Privacy Policy](https://www.cloudflare.com/privacypolicy/) |

Cloudflare Web Analytics uses no cookies and no local storage. It does not track you across sites, and the publisher sees only aggregated counts: page views, referrers, countries, browsers and page load times. It never sees individual visitors. The site's DNS records are not proxied through Cloudflare, so the analytics script is the only way Cloudflare learns of a visit.

The legal basis for these requests is the publisher's legitimate interest (Article 6(1)(f) GDPR) in serving the site, displaying it correctly and knowing which pages are read. The four providers are based in the United States. Transfers to them rely on the EU–U.S. Data Privacy Framework or the safeguards their own policies describe.

## What stays in your browser

The site stores two values in your browser's session storage. Session storage is cleared when you close the tab, and neither value is sent anywhere:

- `pixel:splash` records that the opening animation has played, so it does not play again on every page.
- `gh-stars:Pixel-CLI/pixel` caches the star count, so the navigation bar can show it before the GitHub API answers.

The [savings estimate](/savings/) calculates everything inside the page. The values you enter are neither sent nor stored. A share link puts them in the part of the URL after the `#`, which browsers do not send to the server.

## The pixel binary

This policy covers the website. The `pixel` command-line tool runs on your machine and sends no telemetry or usage data beyond an optional release check. The index it builds and its sidecar files stay on your machine. It connects to the network only for the actions [SECURITY.md](https://github.com/Pixel-CLI/pixel/blob/main/SECURITY.md) lists:

- first-use model downloads from Hugging Face;
- Git remote operations you ask for;
- at most once a day, and only when a command runs at a terminal, one request to GitHub for the latest release tag, so it can tell you an update is out; GitHub sees your IP address and the pixel version, nothing about your code or usage. `PIXEL_NO_UPDATE_CHECK=1` turns it off;
- two opt-in commands that send their input to a service: `pixel classify` sends it to the model endpoint you configure, and `pixel web-search` sends it to the search endpoint you configure, or to DuckDuckGo and Wikipedia when none is configured.

## Your rights

Under the GDPR you have the right to access, correct and erase your personal data, to restrict or object to its processing, and to lodge a complaint with your data protection authority (in France, the [CNIL](https://www.cnil.fr/)). The publisher holds no personal data about visitors. Requests about the logs listed above go to the provider that keeps them, through the policy linked in the table.

## Changes

When this policy changes, this page is updated and the date at the top moves. Its full history is public in the [repository](https://github.com/Pixel-CLI/pixel/commits/main/website/content/legal/privacy-policy.md).
