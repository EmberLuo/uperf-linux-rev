use std::{fs, path::Path};

use tempfile::tempdir;
use uperf_core::{CpuId, Hertz};
use uperf_linux::{LinuxEnvironment, SystemRoots};
use uperf_platform::SysfsIo;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end fixture keeps all discovered resources and assertions co-located"
)]
fn discovers_sparse_cpu_policies_dynamic_opps_and_device_identity() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    create_roots(root);

    let policy0 = root.join("sys/devices/system/cpu/cpufreq/policy0");
    write(&policy0.join("related_cpus"), "0 2\n");
    write(&policy0.join("cpuinfo_min_freq"), "300000\n");
    write(&policy0.join("cpuinfo_max_freq"), "2000000\n");
    write(&policy0.join("scaling_min_freq"), "300000\n");
    write(&policy0.join("scaling_max_freq"), "2000000\n");
    write(&policy0.join("scaling_cur_freq"), "500000\n");
    write(&policy0.join("scaling_governor"), "schedutil\n");
    let many_opps = (0..40)
        .map(|index| (300_000 + index * 40_000).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    write(&policy0.join("scaling_available_frequencies"), &many_opps);

    let policy7 = root.join("sys/devices/system/cpu/cpufreq/policy7");
    write(&policy7.join("related_cpus"), "7 128\n");
    write(&policy7.join("cpuinfo_min_freq"), "500000\n");
    write(&policy7.join("cpuinfo_max_freq"), "3000000\n");
    write(&policy7.join("scaling_min_freq"), "500000\n");
    write(&policy7.join("scaling_max_freq"), "3000000\n");
    write(&policy7.join("scaling_cur_freq"), "600000\n");

    let gpu = root.join("sys/class/devfreq/3d00000.gpu");
    write(&gpu.join("name"), "kgsl-3d0\n");
    write(&gpu.join("min_freq"), "295000000\n");
    write(&gpu.join("max_freq"), "500000000\n");
    write(&gpu.join("cur_freq"), "400000000\n");
    write(
        &gpu.join("available_frequencies"),
        "220000000 295000000 680000000\n",
    );
    write(&gpu.join("governor"), "simple_ondemand\n");

    let thermal = root.join("sys/class/thermal/thermal_zone3");
    write(&thermal.join("type"), "soc\n");
    write(&thermal.join("temp"), "42500\n");

    let input = root.join("sys/class/input/event3/device");
    write(&input.join("name"), "NVTCapacitiveTouchScreen\n");
    write(&input.join("capabilities/abs"), "260800000000000\n");

    write(
        &root.join("sys/firmware/devicetree/base/model"),
        b"Vendor test SoC\0",
    );
    write(
        &root.join("sys/firmware/devicetree/base/compatible"),
        b"vendor,test-soc\0vendor,test-board\0",
    );

    let environment = LinuxEnvironment::new(SystemRoots::below(root)).unwrap();
    let discovery = environment.discover().unwrap();

    assert_eq!(
        discovery.capabilities.compatible,
        ["vendor,test-soc", "vendor,test-board"]
    );
    assert_eq!(discovery.capabilities.cpu_policies.len(), 2);
    assert_eq!(
        discovery.capabilities.cpu_policies[0]
            .cpus
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [CpuId(0), CpuId(2)]
    );
    assert!(
        discovery.capabilities.cpu_policies[0]
            .available_frequencies
            .len()
            > 32
    );
    assert!(
        discovery.capabilities.cpu_policies[1]
            .available_frequencies
            .is_empty(),
        "a missing OPP table represents a continuous range"
    );
    assert_eq!(
        discovery.capabilities.cpu_policies[0].limits.min,
        Hertz(300_000_000)
    );
    assert_eq!(discovery.capabilities.devfreq_targets.len(), 1);
    assert_eq!(
        discovery.capabilities.devfreq_targets[0].limits.min,
        Hertz(220_000_000)
    );
    assert_eq!(
        discovery.capabilities.devfreq_targets[0].limits.max,
        Hertz(680_000_000)
    );
    assert_eq!(discovery.capabilities.thermal_zones.len(), 1);
    assert!(discovery.capabilities.input_devices[0].multi_touch);

    let report = environment.probe().unwrap();
    assert_eq!(report.cpu_times.cpus.len(), 2);
    assert_eq!(report.thermal[0].reading.temperature.unwrap().0, 42_500);
    assert!(report.warnings.is_empty());

    // The environment used by all probes is permanently read-only.
    assert!(
        environment
            .sysfs()
            .write_string(
                Path::new("/sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq"),
                "400000",
            )
            .is_err()
    );
}

#[test]
fn discovers_devices_tree_devfreq_when_class_is_absent() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    create_roots(root);

    let gpu = root.join("sys/devices/platform/soc@0/3d00000.gpu/devfreq/3d00000.gpu");
    write(&gpu.join("name"), "kgsl-3d0\n");
    write(&gpu.join("min_freq"), "220000000\n");
    write(&gpu.join("max_freq"), "680000000\n");
    write(&gpu.join("cur_freq"), "220000000\n");
    write(&gpu.join("available_frequencies"), "220000000 680000000\n");
    write(
        &gpu.join("device/of_node/compatible"),
        b"qcom,adreno\0qcom,adreno-gpu\0",
    );

    let ufs = root.join("sys/devices/platform/soc@0/1d84000.ufshc/devfreq/1d84000.ufshc");
    write(&ufs.join("name"), "ufs-clk\n");
    write(&ufs.join("min_freq"), "100000000\n");
    write(&ufs.join("max_freq"), "400000000\n");
    write(&ufs.join("cur_freq"), "100000000\n");
    write(
        &ufs.join("device/of_node/compatible"),
        b"vendor,test-ufshc\0",
    );

    let environment = LinuxEnvironment::new(SystemRoots::below(root)).unwrap();
    let discovery = environment.discover().unwrap();

    // Discovery reports generic devfreq capabilities. Device configuration,
    // not a filename heuristic, decides which one is a GPU control target.
    assert_eq!(discovery.capabilities.devfreq_targets.len(), 2);
    assert_eq!(
        discovery
            .capabilities
            .devfreq_targets
            .iter()
            .map(|target| target.device_name.as_str())
            .collect::<Vec<_>>(),
        ["ufs-clk", "kgsl-3d0"]
    );
    assert_eq!(
        discovery.capabilities.devfreq_targets[0].compatible,
        ["vendor,test-ufshc"]
    );
    assert_eq!(
        discovery.capabilities.devfreq_targets[1].compatible,
        ["qcom,adreno", "qcom,adreno-gpu"]
    );
    assert!(
        discovery
            .frequency_targets
            .values()
            .all(|paths| paths.minimum.starts_with("/sys/devices/"))
    );
}

#[test]
fn reports_compatible_values_without_a_chip_registry() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    create_roots(root);
    write(
        &root.join("sys/firmware/devicetree/base/compatible"),
        b"vendor,next-soc-gpu\0vendor,next-soc\0vendor,test-board\0",
    );

    let environment = LinuxEnvironment::new(SystemRoots::below(root)).unwrap();
    let discovery = environment.discover().unwrap();

    assert_eq!(
        discovery.capabilities.compatible,
        [
            "vendor,next-soc-gpu",
            "vendor,next-soc",
            "vendor,test-board"
        ]
    );
}

fn create_roots(root: &Path) {
    for directory in ["sys", "proc", "etc"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    write(
        &root.join("proc/stat"),
        "cpu 100 0 20 400 0 0 0 0\ncpu0 50 0 10 200 0 0 0 0\ncpu7 50 0 10 200 0 0 0 0\n",
    );
    write(&root.join("proc/sys/kernel/osrelease"), "7.1-test\n");
    write(
        &root.join("proc/sys/kernel/random/boot_id"),
        "00000000-0000-0000-0000-000000000001\n",
    );
    write(
        &root.join("etc/os-release"),
        "ID=ubuntu\nPRETTY_NAME=\"Ubuntu Test\"\n",
    );
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}
