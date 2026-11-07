# Partner Center submission checklist

Everything on this list is free. Items marked (you) need your own login and cannot be done for you.

## One time

- [ ] (you) Create the free Microsoft developer account as an individual: https://partner.microsoft.com/dashboard
      (Microsoft made individual registration free in Sept 2025.) Follow the verification steps it shows.
- [ ] (you) In Partner Center: Apps and games, New product, MSIX or PWA app, reserve the name **Perennos Tempo**.
      (A reservation lasts a limited time; submit before it runs out.)
- [ ] (you) Copy the three values from the app's Product identity page (Product management, Product identity):
      `Package/Identity/Name`, `Package/Identity/Publisher`, `Package/Properties/PublisherDisplayName`.
- [ ] (you) Host `privacy.html` at a public address that needs no login. Free options:
      GitHub Pages on the public repository (a `docs/privacy.html` file, then Settings, Pages), or a page on perennos.com.
      Open the address in a private browser window to prove it works. Check the contact line at the bottom.
- [ ] (you) Decide the support contact (an address or page that someone reads).

## Build the package (on a Windows machine with the free Windows SDK)

- [ ] `build-msix.ps1 -Store -IdentityName "<Name>" -Publisher "<Publisher>" -PublisherDisplayName "<PublisherDisplayName>" -Apps vault,studio,room`
      (the source-available edition for the Store; see `../msix/README.md`).
- [ ] The version's last number is 0 and the first is 1 or higher (`-Version 1.0.0.0`) if Partner Center objects to 0.x.
- [ ] Install the signed build of the same sources on a clean PC or VM once (the own-machines path) and walk the
      test steps in `listing.md` ("How to test") before you upload. Untested here: needs Windows.

## The submission

- [ ] Pricing and availability: Free, all markets, public. Choose "Publish manually" if you want to pick the moment.
- [ ] Properties: category Productivity (see `listing.md`), privacy policy URL, website, support contact.
- [ ] Age rating: answer the questionnaire with the answers in `listing.md`.
- [ ] Packages: upload the `.msix` (or the `.msixupload`). Leave it unsigned: the Store signs it.
- [ ] Store listing, English: short description, description, features, search terms, release notes, copyright line
      (`listing.md`); screenshots and the three pictures (`screenshots.md`, `art/`).
- [ ] Additional license terms: paste from `listing.md`.
- [ ] Submission options, Notes for certification: paste the text in `listing.md` (this is where the two restricted
      capabilities, runFullTrust and unvirtualizedResources, are justified). If it asks for a reason for each
      restricted capability, give the same two paragraphs.
- [ ] Check once more: no personal names, machine names or private paths in any text or picture.
- [ ] Submit. Certification usually takes up to a few business days; Partner Center emails the result.

## If it comes back refused

- Read the reason. Most are about a missing privacy policy page, a screenshot, or a capability justification.
- If the reason is the `unvirtualizedResources` capability: `../msix/README.md`, "If the Store says no".
- Fix, rebuild (raise the version's third number), upload again.

## After it is live

- [ ] Install it from the Store on a clean PC. Open each tile. Run `tempo-launch --version` in a terminal.
- [ ] Updates: build a new version (higher number), upload it as a new submission; users get it automatically.
