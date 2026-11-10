// Where Pantheon lives. Builds are published on GitHub Releases; update the notes
// below once release builds are signed (Apple notarization, Authenticode).
const repoUrl = 'https://github.com/k1ng0mar/pantheon';

export const project = {
  repo: repoUrl,
  clone: `${repoUrl}.git`,
  releases: `${repoUrl}/releases/latest`,
  issues: `${repoUrl}/issues`,
  security: `${repoUrl}/security/policy`,
};

// Direct downloads: release assets keep these names, so /releases/latest/download/ always
// points at the newest build. Each has a matching .sha256 beside it.
const latest = `${repoUrl}/releases/latest/download`;
export const platformDownloads = [
  { label: 'macOS', detail: 'Apple silicon', href: `${latest}/Pantheon-macOS-arm64.dmg` },
  { label: 'macOS', detail: 'Intel', href: `${latest}/Pantheon-macOS-x64.dmg` },
  { label: 'Windows', detail: 'x64', href: `${latest}/Pantheon-Windows-x64-setup.exe` },
  { label: 'Windows', detail: 'arm64', href: `${latest}/Pantheon-Windows-arm64-setup.exe` },
  { label: 'Linux', detail: 'x86_64', href: `${latest}/Pantheon-Linux-x86_64.AppImage` },
  { label: 'Linux', detail: 'aarch64', href: `${latest}/Pantheon-Linux-aarch64.AppImage` },
] as const;

export const distribution = {
  platforms: 'macOS · Windows · Linux',
  isUnsignedPreview: true,
  requirements: 'Apple silicon or Intel · macOS 12+',
  previewNotice: 'Not notarized by Apple. macOS blocks the first launch.',
  unblockCommand: 'xattr -dr com.apple.quarantine /Applications/Pantheon.app',
  appleSupportUrl: 'https://support.apple.com/en-us/102445',
};
