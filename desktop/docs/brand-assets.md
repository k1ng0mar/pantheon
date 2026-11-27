# Brand assets

Pantheon ships one mark: `src/assets/providers/pantheon.svg`. It is original
vector artwork created for Pantheon. The app icon (`public/icon.svg`) and the
interface mark in `src/dom.ts` are also original artwork. Native icons are
generated from the app SVG with the Tauri icon command.

`src/provider-logos.ts` resolves the mark; Vite bundles it and rendering makes
no remote brand-asset requests. Decorative images have empty alt text and
`aria-hidden="true"`; adjacent text supplies the accessible name. Keep the
mark proportional and preserve its colors when changing the interface.

Historical note: earlier releases shipped logos copied from their owners.
Those integrations and their local assets have since been removed. The marks remain the property
of their respective owners; this project's source license never granted
trademark rights to them.
