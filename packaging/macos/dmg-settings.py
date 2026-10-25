# dmgbuild's settings for RealTimeX Link's .dmg (packaging/macos/dmg.sh passes -D app=…): the app
# and Applications side by side to drag it there, as electron-builder lays out RealTimeX's, and
# the app's icon as the volume's.
import os.path

app = defines["app"]  # noqa: F821 (dmgbuild's)
format = "UDZO"
filesystem = "HFS+"
files = [(app, "RealTimeX Link.app")]
symlinks = {"Applications": "/Applications"}
icon = os.path.join(app, "Contents", "Resources", "icon.icns")
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
window_rect = ((200, 120), (540, 380))
icon_size = 80
text_size = 12
icon_locations = {"RealTimeX Link.app": (130, 220), "Applications": (410, 220)}
