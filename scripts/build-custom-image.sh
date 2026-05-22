#!/bin/bash
# shellcheck disable=SC2046,SC2086,SC2155

set -ex

#VFIO_CUSTOM_IMAGE="-vfio"

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        IMAGE_ARCH="amd64"
        ;;
    aarch64)
        IMAGE_ARCH="arm64"
        ;;
    *)
        echo "Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

install_deps() {
    if command -v apt-get &>/dev/null; then
        sudo apt-get update
        sudo apt-get install -y qemu-utils wget
        if [ -n "$VFIO_CUSTOM_IMAGE" ]; then
            sudo apt-get install -y gdisk parted e2fsprogs
        fi
    elif command -v dnf &>/dev/null; then
        sudo dnf install -y qemu-img wget
        if [ -n "$VFIO_CUSTOM_IMAGE" ]; then
            sudo dnf install -y gdisk parted e2fsprogs
        fi
    elif command -v yum &>/dev/null; then
        sudo yum install -y qemu-img wget
        if [ -n "$VFIO_CUSTOM_IMAGE" ]; then
            sudo yum install -y gdisk parted e2fsprogs
        fi
    else
        echo "Unsupported package manager"
        exit 1
    fi
}

install_deps

mkdir -p custom-image
pushd custom-image || exit

export IMAGE_NAME_BASE=noble-server-cloudimg-${IMAGE_ARCH}

cleanup() {
    mountpoint -q mnt/sys && sudo umount mnt/sys
    mountpoint -q mnt/dev/pts && sudo umount mnt/dev/pts
    mountpoint -q mnt/proc && sudo umount mnt/proc
    mountpoint -q mnt && sudo umount mnt
    EXISTING_LOOP=$(losetup -j "$PWD/$IMAGE_NAME_BASE.raw" -O NAME --noheadings 2>/dev/null | head -1)
    if [ -n "$EXISTING_LOOP" ]; then
        sudo losetup -d "$EXISTING_LOOP"
    fi
}
cleanup

wget -N https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-${IMAGE_ARCH}.img
qemu-img convert -p -f qcow2 -O raw $IMAGE_NAME_BASE.img $IMAGE_NAME_BASE.raw
if [ -n "$VFIO_CUSTOM_IMAGE" ]; then
    qemu-img resize -f raw "$IMAGE_NAME_BASE.raw" 10G
    sudo sgdisk -e "$IMAGE_NAME_BASE.raw"
    sudo parted "$IMAGE_NAME_BASE.raw" resizepart 1 10737MB
fi
mkdir -p mnt
LOOP_DEV=$(sudo losetup --partscan -f --show $IMAGE_NAME_BASE.raw)
export ROOTFS="${LOOP_DEV}p1"
if [ -n "$VFIO_CUSTOM_IMAGE" ]; then
    sudo e2fsck -f "$ROOTFS"
    sudo resize2fs "$ROOTFS"
fi
sudo mount $ROOTFS mnt
sudo mv mnt/etc/resolv.conf mnt/etc/resolv.conf.backup
sudo cp -L /etc/resolv.conf mnt/etc/resolv.conf

touch extra_commands

GUEST_PACKAGES="fio iperf iperf3 socat stress tpm2-tools kexec-tools"
if [ "$IMAGE_ARCH" = "amd64" ]; then
    GUEST_PACKAGES="$GUEST_PACKAGES cpuid"
fi

if [ -n "$VFIO_CUSTOM_IMAGE" ] && [ "$IMAGE_ARCH" = "amd64" ]; then
    cat >extra_commands <<EOF
    wget https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
    sudo dpkg -i cuda-keyring_1.1-1_all.deb
    sudo apt-get update
    sudo apt-get -y install cuda-drivers
EOF
fi

APT_PROXY_CONF="APT::Sandbox::User \"root\";
"
if [ -n "${http_proxy:-$HTTP_PROXY}" ]; then
    APT_PROXY_CONF="${APT_PROXY_CONF}Acquire::http::Proxy \"${http_proxy:-$HTTP_PROXY}\";
"
fi
if [ -n "${https_proxy:-$HTTPS_PROXY}" ]; then
    APT_PROXY_CONF="${APT_PROXY_CONF}Acquire::https::Proxy \"${https_proxy:-$HTTPS_PROXY}\";
"
fi

echo "$APT_PROXY_CONF" | sudo tee mnt/etc/apt/apt.conf.d/99proxy >/dev/null

cat >script <<EOF
#!/bin/bash
set -xe
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devpts devpts /dev/pts
export DEBIAN_FRONTEND=noninteractive
apt update
apt install -y $GUEST_PACKAGES
apt remove -y --purge snapd pollinate
source extra_commands
umount /dev/pts
umount /sys
umount /proc
history -c
exit
EOF

sudo cp script extra_commands mnt
sudo chmod +x mnt/script
sudo chroot mnt ./script
sudo rm -f mnt/etc/apt/apt.conf.d/99proxy
sudo mv mnt/etc/resolv.conf.backup mnt/etc/resolv.conf
sudo umount mnt
sudo losetup -d "$LOOP_DEV"
cp $IMAGE_NAME_BASE.raw $IMAGE_NAME_BASE-custom$VFIO_CUSTOM_IMAGE-$(date "+%Y%m%d")-0.raw
qemu-img convert -p -f raw -O qcow2 $IMAGE_NAME_BASE-custom$VFIO_CUSTOM_IMAGE-$(date "+%Y%m%d")-0.raw $IMAGE_NAME_BASE-custom$VFIO_CUSTOM_IMAGE-$(date "+%Y%m%d")-0.qcow2
popd || exit
