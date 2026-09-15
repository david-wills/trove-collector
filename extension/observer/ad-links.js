// Trove page observer — shared ad-link extraction.
//
// One home for "given a creative's <a href>, what's the advertiser landing
// URL (and the AdChoices transparency URL)?" so the detector (reading
// same-origin friendly iframes) and the inspector (injected into cross-origin
// ad frames) derive identical results and cannot drift. Loaded after
// observer/ad-domains.js, which provides TROVE_AD_CLICK_PARAMS / troveAdDomainOf.
// Defines globals only.

// Google's AdChoices / "Why this ad?" transparency link. It names the
// advertiser, but only once *fetched* — so it is never a landing URL.
function troveWhyFrom(href) {
  let u;
  try {
    u = new URL(href);
  } catch {
    return '';
  }
  return u.protocol === 'https:' &&
    u.hostname === 'adssettings.google.com' &&
    u.pathname.includes('/whythisad')
    ? u.href
    : '';
}

// The advertiser's own landing URL hiding in a click-through href. Click
// redirects nest (Google's pcs/click carries adurl=, whose value is often
// *another* ad-infra redirect), so unwrap known params depth-first. The
// result is a landing only if it lands off ad infrastructure — a chain that
// dead-ends on a tracker (e.g. ad.doubleclick.net/ddm/trackclk) names no
// advertiser, so it yields '' rather than the network's own domain. `href`
// is expected absolute (read from an element's `.href`, already resolved).
function troveLandingFrom(href, depth) {
  depth = depth || 0;
  let u;
  try {
    u = new URL(href);
  } catch {
    return '';
  }
  if (u.protocol !== 'https:' && u.protocol !== 'http:') return '';
  if (troveWhyFrom(u.href)) return ''; // AdChoices is never the advertiser
  if (depth < 4) {
    for (const p of TROVE_AD_CLICK_PARAMS) {
      const v = u.searchParams.get(p);
      if (v && (v.startsWith('https://') || v.startsWith('http://'))) {
        return troveLandingFrom(v, depth + 1);
      }
    }
  }
  return troveAdDomainOf(u.hostname) ? '' : u.href;
}

// Scan a creative's document for its advertiser landing + transparency link.
// Returns { landing, why } (either may be ''). Used by the detector on
// same-origin friendly iframes and by the inspector on its own frame.
function troveScanLinks(doc) {
  let landing = '';
  let why = '';
  let anchors;
  try {
    anchors = doc.querySelectorAll('a[href]');
  } catch {
    return { landing, why };
  }
  for (const a of anchors) {
    if (!why) why = troveWhyFrom(a.href);
    if (!landing) landing = troveLandingFrom(a.href);
    if (landing && why) break;
  }
  return { landing, why };
}
