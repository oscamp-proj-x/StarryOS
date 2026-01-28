# =================================================================
# StarryOS RK3588 Deployment System
# Author: wang lian <lianux.mm@gmail.com> & Gemini
# =================================================================

# -----------------------------------------------------------------
# Path Definitions
# -----------------------------------------------------------------
ROOT_DIR     := $(shell pwd)
TOOLS_DIR    := $(ROOT_DIR)/tools
OUT_DIR      := $(ROOT_DIR)/out
ARCEOS_DIR   := $(ROOT_DIR)/arceos

# Build Artifacts
KERNEL_UIMG  := $(ROOT_DIR)/StarryOS_aarch64-rk3588.uimg
DTB_FILE     := $(TOOLS_DIR)/rk3588-orangepi-5-plus.dtb
BOOT_CMD     := $(TOOLS_DIR)/boot.cmd
BOOT_SCR     := $(OUT_DIR)/boot.scr
BOOT_IMG     := $(OUT_DIR)/boot.img
ROOT_IMG     := $(ARCEOS_DIR)/disk.img

# Flash Tooling (Rockchip Specific)
RK_TOOL      := sudo rkdeveloptool
LOADER_BIN   := $(TOOLS_DIR)/MiniLoaderAll.bin
PARAM_TXT    := $(TOOLS_DIR)/parameter.txt

# UI Terminal Colors
GREEN  := \033[0;32m
CYAN   := \033[0;36m
YELLOW := \033[0;33m
RED    := \033[0;31m
NC     := \033[0m


# -----------------------------------------------------------------
# Verification Target: Check Path Definitions
# -----------------------------------------------------------------
.PHONY: check-path

check-path:
	@echo "$(YELLOW)===== StarryOS RK3588 Path Check =====$(NC)"
	@echo "Current Workdir:   $(CURDIR)"
	@echo "---------------------------------------"
	@echo "$(CYAN)[Build Artifacts]$(NC)"
	@printf "Kernel (.uimg):    %-40s " "$(KERNEL_UIMG)"
	@[ -f "$(KERNEL_UIMG)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
	@printf "Rootfs (.img):     %-40s " "$(ROOT_IMG)"
	@[ -f "$(ROOT_IMG)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
    
	@echo "\n$(CYAN)[Hardware/Tooling]$(NC)"
	@printf "Device Tree (DTB): %-40s " "$(DTB_FILE)"
	@[ -f "$(DTB_FILE)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
	@printf "Parameter File:    %-40s " "$(PARAM_TXT)"
	@[ -f "$(PARAM_TXT)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
	@printf "MiniLoader Bin:    %-40s " "$(LOADER_BIN)"
	@[ -f "$(LOADER_BIN)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
    
	@echo "\n$(CYAN)[Intermediate/Output]$(NC)"
	@printf "Boot Command:      %-40s " "$(BOOT_CMD)"
	@[ -f "$(BOOT_CMD)" ] && echo "$(GREEN)[OK]$(NC)" || echo "$(RED)[MISSING]$(NC)"
	@printf "Output Boot Img:   %-40s " "$(BOOT_IMG)"
	@[ -f "$(BOOT_IMG)" ] && echo "$(YELLOW)[GEN-TARGET]$(NC)" || echo "$(YELLOW)[WAIT-BUILD]$(NC)"
	@echo "$(YELLOW)=======================================$(NC)"

.PHONY: rk-image rk-flash rk-clean deploy

deploy: rk-image rk-flash

# -----------------------------------------------------------------
# Image Creation: Using shell variables instead of $(eval)
# -----------------------------------------------------------------
rk-image:
	@echo "$(CYAN)[IMAGE] Validating build environment...$(NC)"
	@which mkfs.ext4 mkimage resize2fs e2fsck > /dev/null || (echo "$(RED)Error: Missing tools.$(NC)" && exit 1)
	@if [ ! -f "$(KERNEL_UIMG)" ]; then echo "$(RED)Error: Kernel $(KERNEL_UIMG) not found!$(NC)"; exit 1; fi
	@mkdir -p $(OUT_DIR)
	@echo "$(CYAN)[IMAGE] Generating boot.scr...$(NC)"
	@mkimage -A arm -T script -C none -n "TF boot" -d $(BOOT_CMD) $(BOOT_SCR) > /dev/null
	@echo "$(CYAN)[IMAGE] Allocating 128MB sparse image container...$(NC)"
	@dd if=/dev/zero of=$(BOOT_IMG) bs=1M count=128 status=none
	@mkfs.ext4 -F -q -L "STARRY_BOOT" $(BOOT_IMG)
	@echo "$(CYAN)[IMAGE] Injecting artifacts (using sudo)...$(NC)"
	@MNT_TMP=$$(mktemp -d); \
	sudo mount -o loop $(BOOT_IMG) $$MNT_TMP; \
	sudo cp $(BOOT_SCR) $$MNT_TMP/; \
	sudo cp $(KERNEL_UIMG) $$MNT_TMP/kernel.uimg; \
	sudo cp $(DTB_FILE) $$MNT_TMP/rk3588-orangepi-5-plus.dtb; \
	sudo umount $$MNT_TMP; \
	rmdir $$MNT_TMP
	@echo "$(CYAN)[IMAGE] Optimizing image size (resize2fs)...$(NC)"
	@e2fsck -f -y $(BOOT_IMG) > /dev/null
	@resize2fs -M $(BOOT_IMG) > /dev/null
	@echo "$(GREEN)[SUCCESS] Image ready: $(BOOT_IMG) ($$(du -h $(BOOT_IMG) | cut -f1))$(NC)"

# -----------------------------------------------------------------
# Flashing logic
# -----------------------------------------------------------------
rk-flash:
	@echo "$(YELLOW)[FLASH] Polling for Maskrom device...$(NC)"
	@while ! $(RK_TOOL) ld | grep -q "Maskrom"; do sleep 1; done
	@echo "$(CYAN)[FLASH] Initializing Download-Boot (db)...$(NC)"
	@$(RK_TOOL) db $(LOADER_BIN)
	@sleep 2
	@echo "$(CYAN)[FLASH] Selecting Storage: SD Card (cs 2)...$(NC)"
	@$(RK_TOOL) cs 2
	@echo "$(CYAN)[FLASH] Synchronizing GPT Table...$(NC)"
	@$(RK_TOOL) gpt $(PARAM_TXT)
	@$(RK_TOOL) ppt
	@sleep 1
	@echo "$(CYAN)[FLASH] Writing BOOT...$(NC)"
	@$(RK_TOOL) wlx boot $(BOOT_IMG)
	@echo "$(CYAN)[FLASH] Writing ROOT...$(NC)"
	@if [ -f "$(ROOT_IMG)" ]; then $(RK_TOOL) wlx root $(ROOT_IMG); else echo "$(YELLOW)Skip: Rootfs not found.$(NC)"; fi
	@echo "$(GREEN)[SUCCESS] Deployment complete. Resetting...$(NC)"
	@$(RK_TOOL) rd

rk-clean:
	@rm -rf $(OUT_DIR)
	@echo "$(GREEN)[CLEAN] Workspace cleaned.$(NC)"