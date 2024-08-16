{ pkgs }:

rec {
  pkg_a = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_a";
    buildInputs = [ ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_b = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_b";
    buildInputs = [ pkg_a ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_c = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_c";
    buildInputs = [ pkg_b ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_d = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_d";
    buildInputs = [ pkg_c ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_e = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_e";
    buildInputs = [ ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_f = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_f";
    buildInputs = [ pkg_e ];
    unpackPhase = "false";
    installPhase = ''mkdir -p $out'';
  };
  pkg_g = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_g";
    buildInputs = [ pkg_f ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
  pkg_h = pkgs.stdenvNoCC.mkDerivation {
    name = "pkg_h";
    buildInputs = [ pkg_g ];
    unpackPhase = "true";
    installPhase = ''mkdir -p $out'';
  };
}
