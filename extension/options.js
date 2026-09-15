// Trove page observer — options page.
//
// The master toggle drives the real enforcement: Chrome's optional host
// permission. Enabling calls permissions.request() from the click handler
// (user-gesture requirement) so Chrome shows its own consent prompt;
// disabling removes the grant — off means the extension provably cannot
// read pages, verifiable in chrome://extensions. Script (un)registration is
// the service worker's job (reconcileObserver reacts to storage changes);
// this page only writes desired state and reflects ground truth.

const OBSERVER_ORIGINS = { origins: ['<all_urls>'] };
const DEFAULTS = { enabled: false, features: { ads: false } };

const master = document.getElementById('master');
const ads = document.getElementById('ads');
const features = document.getElementById('features');
const status = document.getElementById('status');

async function readState() {
  const { pageObserver } = await chrome.storage.local.get('pageObserver');
  return {
    ...DEFAULTS,
    ...pageObserver,
    features: { ...DEFAULTS.features, ...(pageObserver && pageObserver.features) },
  };
}

async function render() {
  const s = await readState();
  // The status line reflects what Chrome reports, not stored intent — if the
  // user revoked access via chrome://extensions, this shows it.
  const granted = await chrome.permissions.contains(OBSERVER_ORIGINS);
  master.checked = s.enabled && granted;
  features.disabled = !(s.enabled && granted);
  ads.checked = s.features.ads;
  status.textContent = granted
    ? 'Chrome reports: site access granted. Observation runs only for the features enabled below.'
    : 'Chrome reports: no site access — the extension cannot read any page.';
  status.classList.toggle('granted', granted);
}

master.addEventListener('change', async () => {
  if (master.checked) {
    // First await in the gesture handler: anything earlier risks spending
    // the transient user activation Chrome requires for the prompt.
    const ok = await chrome.permissions.request(OBSERVER_ORIGINS);
    const s = await readState();
    if (ok) await chrome.storage.local.set({ pageObserver: { ...s, enabled: true } });
  } else {
    const s = await readState();
    await chrome.storage.local.set({ pageObserver: { ...s, enabled: false } });
    await chrome.permissions.remove(OBSERVER_ORIGINS);
  }
  render();
});

ads.addEventListener('change', async () => {
  const s = await readState();
  await chrome.storage.local.set({
    pageObserver: { ...s, features: { ...s.features, ads: ads.checked } },
  });
  render();
});

// Reflect out-of-band changes (revocation in chrome://extensions, another
// options window) while this page is open.
chrome.permissions.onAdded.addListener(render);
chrome.permissions.onRemoved.addListener(render);
chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'local' && changes.pageObserver) render();
});

render();
