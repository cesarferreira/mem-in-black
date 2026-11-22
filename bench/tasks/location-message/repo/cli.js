const { t } = require("./i18n");

const amount = Number(process.argv[2]);
if (Number.isNaN(amount)) {
  console.error(t("bad_number"));
  process.exit(2);
}
if (amount < 0) {
  console.error(t("negative"));
  process.exit(2);
}
console.log(amount.toFixed(2));
