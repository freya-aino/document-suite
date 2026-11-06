# vs-ingest: PDF rasterization and text layer via pdfium (pdfium-render, dynamic binding).
{ pkgs, ... }:

{
  packages = [ pkgs.pdfium ];

  env = {
    # Bind with Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(...)).
    PDFIUM_DYNAMIC_LIB_PATH = "${pkgs.pdfium}/lib";
    # Lets Pdfium::bind_to_system_library() find libpdfium.so as it will in the rootfs.
    LD_LIBRARY_PATH = "${pkgs.pdfium}/lib";
  };
}
