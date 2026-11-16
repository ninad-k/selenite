// Word count: registers a command that counts the words of the open page.
// Needs the "read" permission (vault.read) and "commands" (ui.registerCommand).
// `selenite` is provided by the sandbox; there is no `window.parent`, network or storage access.

function countWords(markdown) {
  // Skip the YAML front matter block, then count runs of non-space characters.
  const body = markdown.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n?/, "");
  const words = body.match(/\S+/g);
  return words ? words.length : 0;
}

selenite.ui.registerCommand("count", "Count words in this page", async (context) => {
  if (!context.activePath) {
    await selenite.ui.notify("Open a page first.");
    return;
  }
  const page = await selenite.vault.read(context.activePath);
  const n = countWords(page.text);
  await selenite.ui.notify(`${context.activePath}: ${n} ${n === 1 ? "word" : "words"}`);
});
