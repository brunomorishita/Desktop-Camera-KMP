use std::{
    fs,
    path::{Path, PathBuf},
};

const DRM_CLASS: &str = "/sys/class/drm";

pub(super) struct DrmNode {
    pub render_node: PathBuf,
    pub pci_address: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub kernel_driver: String,
    pub drives_display: bool,
    pub boot_vga: bool,
}

// Render nodes (/dev/dri/renderD*) give access to the GPU without needing to be the
// display master, which is all video decode/encode and offscreen rendering need
pub(super) fn render_nodes() -> Vec<DrmNode> {
    let root = Path::new(DRM_CLASS);
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();

    let mut nodes: Vec<DrmNode> = names
        .iter()
        .filter(|name| name.starts_with("renderD"))
        .filter_map(|name| {
            let device = root.join(name).join("device");
            let pci_address = pci_address(&root.join(name))?;
            Some(DrmNode {
                render_node: Path::new("/dev/dri").join(name),
                vendor_id: read_hex(&device.join("vendor")).unwrap_or(0),
                device_id: read_hex(&device.join("device")).unwrap_or(0),
                kernel_driver: fs::read_link(device.join("driver"))
                    .ok()
                    .and_then(|path| Some(path.file_name()?.to_str()?.to_string()))
                    .unwrap_or_default(),
                drives_display: has_connected_display(root, &names, &pci_address),
                boot_vga: read_trimmed(&device.join("boot_vga")).as_deref() == Some("1"),
                pci_address,
            })
        })
        .collect();

    nodes.sort_by(|a, b| a.render_node.cmp(&b.render_node));
    nodes
}

// The card node (cardN) and the render node of a GPU are both children of its PCI device
fn pci_address(drm_node: &Path) -> Option<String> {
    let device = fs::canonicalize(drm_node.join("device")).ok()?;
    Some(device.file_name()?.to_str()?.to_string())
}

// Connectors are listed as card<N>-<type>-<index>, e.g. card1-eDP-1
fn has_connected_display(root: &Path, names: &[String], pci: &str) -> bool {
    names
        .iter()
        .filter_map(|name| {
            let (card, _) = name.split_once('-')?;
            card.starts_with("card").then_some((card, name))
        })
        .any(|(card, connector)| {
            pci_address(&root.join(card)).as_deref() == Some(pci)
                && read_trimmed(&root.join(connector).join("status")).as_deref()
                    == Some("connected")
        })
}

fn read_trimmed(path: &Path) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}

fn read_hex(path: &Path) -> Option<u16> {
    let text = read_trimmed(path)?;
    u16::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}
