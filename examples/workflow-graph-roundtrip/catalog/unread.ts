export async function unread_subjects() {
  const emails = await gmail.list_recent({ count: 6 });
  const unread = emails.filter((email) => email.unread);
  const subjects = unread.map((email) => email.subject);
  for (const subject of subjects) {
    await display.add_item({ list: "unread", item: subject });
  }
  return { count: subjects.length, subjects };
}
