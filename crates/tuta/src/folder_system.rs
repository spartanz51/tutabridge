use crate::entities::generated::tutanota::{Mail, MailSet};
use tutasdk::IdTupleGenerated;
use crate::GeneratedId;
use num_enum::TryFromPrimitive;
use std::collections::HashMap;

#[derive(Copy, Clone, PartialEq, Eq, TryFromPrimitive, Debug)]
#[repr(u64)]
pub enum MailSetKind {
	Custom = 0,
	Inbox = 1,
	Sent = 2,
	Trash = 3,
	Archive = 4,
	Spam = 5,
	Draft = 6,
	All = 7,
	Label = 8,
	Imported = 9,
	Scheduled = 10,
	Unknown = 9999,
}

pub trait MailSetExt {
 fn bridge_mail_set_kind(&self) -> MailSetKind;
 fn element_id(&self) -> Option<&GeneratedId>;
 fn is_visible_system(&self) -> bool;
}
impl MailSetExt for MailSet {
	fn bridge_mail_set_kind(&self) -> MailSetKind {
		MailSetKind::try_from(self.folderType as u64).unwrap_or(MailSetKind::Unknown)
	}

	/// Element id of this mail set (the `_id` is always set for loaded folders).
	fn element_id(&self) -> Option<&GeneratedId> {
		self._id.as_ref().map(|id| &id.element_id)
	}

	/// System folders that are shown to the user as real folders.
	/// Mirrors TS `isVisibleSystemMailSet`.
	fn is_visible_system(&self) -> bool {
		matches!(
			self.bridge_mail_set_kind(),
			MailSetKind::Inbox
				| MailSetKind::Sent
				| MailSetKind::Trash
				| MailSetKind::Archive
				| MailSetKind::Spam
				| MailSetKind::Draft
				| MailSetKind::Scheduled
		)
	}
}

/// A folder with its (recursively built) children. Mirrors TS `FolderSubtree`.
pub struct FolderSubtree {
	pub folder: MailSet,
	pub children: Vec<FolderSubtree>,
}

/// A folder paired with its depth in the tree. Mirrors TS `IndentedFolder`.
pub struct IndentedFolder<'a> {
	pub level: usize,
	pub folder: &'a MailSet,
}

/// Accessor for the folder trees, rebuilt from the flat `MailSet` list.
/// Mirrors TS `FolderSystem`.
pub struct FolderSystem {
	system_subtrees: Vec<FolderSubtree>,
	custom_subtrees: Vec<FolderSubtree>,
	imported_mail_set: Option<MailSet>,
}

impl FolderSystem {
	#[must_use]
	pub fn new(folders: Vec<MailSet>) -> Self {
		// Group children by their parent's element id.
		let mut children_by_parent: HashMap<GeneratedId, Vec<MailSet>> = HashMap::new();
		for folder in &folders {
			if let Some(parent) = &folder.parentFolder {
				children_by_parent
					.entry(parent.element_id.clone())
					.or_default()
					.push(folder.clone());
			}
		}

		let mut system: Vec<MailSet> = Vec::new();
		let mut top_level_custom: Vec<MailSet> = Vec::new();
		let mut imported_mail_set: Option<MailSet> = None;
		for folder in folders {
			if folder.is_visible_system() {
				system.push(folder);
			} else if folder.bridge_mail_set_kind() == MailSetKind::Custom
				&& folder.parentFolder.is_none()
			{
				top_level_custom.push(folder);
			} else if folder.bridge_mail_set_kind() == MailSetKind::Imported && imported_mail_set.is_none()
			{
				imported_mail_set = Some(folder);
			}
		}

		system.sort_by(compare_system);
		top_level_custom.sort_by(compare_custom);

		let system_subtrees = system
			.into_iter()
			.map(|f| make_subtree(&children_by_parent, f))
			.collect();
		let custom_subtrees = top_level_custom
			.into_iter()
			.map(|f| make_subtree(&children_by_parent, f))
			.collect();

		Self {
			system_subtrees,
			custom_subtrees,
			imported_mail_set,
		}
	}

	/// Search for a specific system folder type. Some mailboxes may not have
	/// every system folder. Mirrors TS `getSystemFolderByType`.
	#[must_use]
	pub fn system_folder_by_type(&self, bridge_mail_set_kind: MailSetKind) -> Option<&MailSet> {
		self.system_subtrees
			.iter()
			.find(|s| s.folder.bridge_mail_set_kind() == bridge_mail_set_kind)
			.map(|s| &s.folder)
	}

	/// All folders (system then custom), flattened with their indentation level.
	/// Mirrors TS `getIndentedList`.
	#[must_use]
	pub fn indented_list(&self) -> Vec<IndentedFolder<'_>> {
		let mut list = Vec::new();
		collect_indented(&self.system_subtrees, 0, &mut list);
		collect_indented(&self.custom_subtrees, 0, &mut list);
		list
	}

	/// Find a folder by its element id, searching both trees. Mirrors TS `getFolderById`.
	#[must_use]
	pub fn folder_by_id(&self, element_id: &GeneratedId) -> Option<&MailSet> {
		self.subtree_by_id(element_id).map(|s| &s.folder)
	}

	/// Find the folder a mail belongs to, via `Mail.sets`. Mirrors TS `getFolderByMail`.
	#[must_use]
	pub fn folder_by_mail(&self, mail: &Mail) -> Option<&MailSet> {
		mail.sets
			.iter()
			.find_map(|set_id| self.folder_by_id(&set_id.element_id))
	}

	/// Immediate children of a parent (custom folders); top-level custom folders
	/// when `parent` is `None`. Mirrors TS `getCustomFoldersOfParent`.
	#[must_use]
	pub fn custom_folders_of_parent(&self, parent: Option<&IdTupleGenerated>) -> Vec<&MailSet> {
		match parent {
			Some(parent) => self
				.subtree_by_id(&parent.element_id)
				.map(|s| s.children.iter().map(|c| &c.folder).collect())
				.unwrap_or_default(),
			None => self.custom_subtrees.iter().map(|s| &s.folder).collect(),
		}
	}

	/// All ancestors of the folder, including the folder itself. Mirrors TS `getPathToFolder`.
	#[must_use]
	pub fn path_to_folder(&self, folder_id: &GeneratedId) -> Vec<&MailSet> {
		path_in_subtrees(&self.system_subtrees, folder_id)
			.or_else(|| path_in_subtrees(&self.custom_subtrees, folder_id))
			.unwrap_or_default()
	}

	/// The imported mail set, if any. Mirrors TS `importedMailSet`.
	#[must_use]
	pub fn imported_mail_set(&self) -> Option<&MailSet> {
		self.imported_mail_set.as_ref()
	}

	fn subtree_by_id(&self, element_id: &GeneratedId) -> Option<&FolderSubtree> {
		subtree_by_id(&self.system_subtrees, element_id)
			.or_else(|| subtree_by_id(&self.custom_subtrees, element_id))
	}
}

fn make_subtree(
	children_by_parent: &HashMap<GeneratedId, Vec<MailSet>>,
	parent: MailSet,
) -> FolderSubtree {
	let children = parent
		.element_id()
		.and_then(|id| children_by_parent.get(id))
		.map(|kids| {
			let mut sorted = kids.clone();
			sorted.sort_by(compare_custom);
			sorted
				.into_iter()
				.map(|child| make_subtree(children_by_parent, child))
				.collect()
		})
		.unwrap_or_default();
	FolderSubtree {
		folder: parent,
		children,
	}
}

fn collect_indented<'a>(
	subtrees: &'a [FolderSubtree],
	level: usize,
	out: &mut Vec<IndentedFolder<'a>>,
) {
	for subtree in subtrees {
		out.push(IndentedFolder {
			level,
			folder: &subtree.folder,
		});
		collect_indented(&subtree.children, level + 1, out);
	}
}

fn subtree_by_id<'a>(
	subtrees: &'a [FolderSubtree],
	element_id: &GeneratedId,
) -> Option<&'a FolderSubtree> {
	for subtree in subtrees {
		if subtree.folder.element_id() == Some(element_id) {
			return Some(subtree);
		}
		if let Some(found) = subtree_by_id(&subtree.children, element_id) {
			return Some(found);
		}
	}
	None
}

fn path_in_subtrees<'a>(
	subtrees: &'a [FolderSubtree],
	element_id: &GeneratedId,
) -> Option<Vec<&'a MailSet>> {
	for subtree in subtrees {
		if subtree.folder.element_id() == Some(element_id) {
			return Some(vec![&subtree.folder]);
		}
		if let Some(mut subpath) = path_in_subtrees(&subtree.children, element_id) {
			subpath.insert(0, &subtree.folder);
			return Some(subpath);
		}
	}
	None
}

/// Display order for visible system folders. Mirrors TS `folderTypeToOrder`.
fn system_order(kind: MailSetKind) -> u8 {
	match kind {
		MailSetKind::Inbox => 0,
		MailSetKind::Draft => 1,
		MailSetKind::Scheduled => 2,
		MailSetKind::Sent => 3,
		MailSetKind::Trash => 4,
		MailSetKind::Archive => 5,
		MailSetKind::Spam => 6,
		_ => 7,
	}
}

fn compare_system(a: &MailSet, b: &MailSet) -> std::cmp::Ordering {
	system_order(a.bridge_mail_set_kind()).cmp(&system_order(b.bridge_mail_set_kind()))
}

fn compare_custom(a: &MailSet, b: &MailSet) -> std::cmp::Ordering {
	a.name.cmp(&b.name)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::entities::generated::tutanota::Mail;
	use crate::fixtures::create_test_entity;

	fn mail_set(elem: &str, kind: MailSetKind, name: &str, parent: Option<&str>) -> MailSet {
		MailSet {
			_id: Some(IdTupleGenerated {
				list_id: GeneratedId("folders".to_owned()),
				element_id: GeneratedId(elem.to_owned()),
			}),
			folderType: kind as i64,
			name: name.to_owned(),
			parentFolder: parent.map(|p| IdTupleGenerated {
				list_id: GeneratedId("folders".to_owned()),
				element_id: GeneratedId(p.to_owned()),
			}),
			..create_test_entity()
		}
	}

	fn id(elem: &str) -> IdTupleGenerated {
		IdTupleGenerated {
			list_id: GeneratedId("folders".to_owned()),
			element_id: GeneratedId(elem.to_owned()),
		}
	}

	#[test]
	fn separates_system_and_custom_and_sorts() {
		let fs = FolderSystem::new(vec![
			mail_set("spam", MailSetKind::Spam, "", None),
			mail_set("inbox", MailSetKind::Inbox, "", None),
			mail_set("zeta", MailSetKind::Custom, "Zeta", None),
			mail_set("alpha", MailSetKind::Custom, "Alpha", None),
		]);

		let systems: Vec<_> = fs
			.indented_list()
			.into_iter()
			.filter(|f| f.folder.is_visible_system())
			.collect();
		assert_eq!(systems[0].folder.bridge_mail_set_kind(), MailSetKind::Inbox);
		assert_eq!(systems[1].folder.bridge_mail_set_kind(), MailSetKind::Spam);

		let customs = fs.custom_folders_of_parent(None);
		assert_eq!(
			customs.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
			vec!["Alpha", "Zeta"]
		);
	}

	#[test]
	fn builds_nested_custom_tree_with_indent() {
		let fs = FolderSystem::new(vec![
			mail_set("inbox", MailSetKind::Inbox, "", None),
			mail_set("parent", MailSetKind::Custom, "Parent", None),
			mail_set("child", MailSetKind::Custom, "Child", Some("parent")),
			mail_set("grandchild", MailSetKind::Custom, "Grandchild", Some("child")),
		]);

		let children = fs.custom_folders_of_parent(Some(&id("parent")));
		assert_eq!(children.len(), 1);
		assert_eq!(children[0].name, "Child");

		let indented = fs.indented_list();
		let parent = indented.iter().find(|f| f.folder.name == "Parent").unwrap();
		let child = indented.iter().find(|f| f.folder.name == "Child").unwrap();
		let grand = indented.iter().find(|f| f.folder.name == "Grandchild").unwrap();
		assert_eq!(child.level, parent.level + 1);
		assert_eq!(grand.level, parent.level + 2);
	}

	#[test]
	fn folder_by_id_finds_nested() {
		let fs = FolderSystem::new(vec![
			mail_set("parent", MailSetKind::Custom, "Parent", None),
			mail_set("child", MailSetKind::Custom, "Child", Some("parent")),
		]);
		assert_eq!(
			fs.folder_by_id(&GeneratedId("child".to_owned()))
				.map(|f| f.name.as_str()),
			Some("Child")
		);
		assert!(fs.folder_by_id(&GeneratedId("missing".to_owned())).is_none());
	}

	#[test]
	fn path_to_folder_returns_ancestors_then_self() {
		let fs = FolderSystem::new(vec![
			mail_set("parent", MailSetKind::Custom, "Parent", None),
			mail_set("child", MailSetKind::Custom, "Child", Some("parent")),
		]);
		let path: Vec<_> = fs
			.path_to_folder(&GeneratedId("child".to_owned()))
			.into_iter()
			.map(|f| f.name.clone())
			.collect();
		assert_eq!(path, vec!["Parent", "Child"]);
	}

	#[test]
	fn folder_by_mail_uses_sets() {
		let fs = FolderSystem::new(vec![mail_set("inbox", MailSetKind::Inbox, "", None)]);
		let mail = Mail {
			sets: vec![id("inbox")],
			..create_test_entity()
		};
		assert_eq!(
			fs.folder_by_mail(&mail).map(|f| f.bridge_mail_set_kind()),
			Some(MailSetKind::Inbox)
		);
	}

	#[test]
	fn label_and_imported_are_not_folders() {
		let fs = FolderSystem::new(vec![
			mail_set("inbox", MailSetKind::Inbox, "", None),
			mail_set("label1", MailSetKind::Label, "Work", None),
			mail_set("imp", MailSetKind::Imported, "Imported", None),
		]);
		assert!(fs.custom_folders_of_parent(None).is_empty());
		assert_eq!(
			fs.imported_mail_set().map(|f| f.bridge_mail_set_kind()),
			Some(MailSetKind::Imported)
		);
		assert!(fs.folder_by_id(&GeneratedId("label1".to_owned())).is_none());
	}

	#[test]
	fn bridge_mail_set_kind_maps_new_variants() {
		assert_eq!(MailSetKind::try_from(8u64), Ok(MailSetKind::Label));
		assert_eq!(MailSetKind::try_from(9u64), Ok(MailSetKind::Imported));
		assert_eq!(MailSetKind::try_from(10u64), Ok(MailSetKind::Scheduled));
	}
}
